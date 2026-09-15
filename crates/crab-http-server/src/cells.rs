use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::Duration,
};

use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, ApplicationIdentityStore, BuildDescriptor, CatalogRole,
    CellAuthority, CellCatalog, CellId, CellModule, CellRuntime, CellTarget, ControlState, Digest,
    EffectModule, MaintenanceModule, MigrationDescriptor, MigrationFailure, MigrationProgressState,
    MigrationProgressStore, ModuleDescriptor, NamespaceDescriptor, NamespaceId, NodeAdvertisement,
    NodeCapacity, NodeDirectory, OperationDescriptor, Owner, PeerRoundTrip, PeerSigner, Registry,
    RegistryBuilder, ReleaseRecord, ReleaseState, ReleaseStore, RequestId, SessionId,
    SqlWorkerPool, TenantId, VersionedNodeAdvertisement, register_effect_delivery,
    register_maintenance,
};
use crab_storage::CellStorageLayout;
use ed25519_dalek::SigningKey;
use object_store::path::Path;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{Config, Error, Result, storage_root::StorageRoot};

mod initializer;
pub(crate) mod repository;
mod router;
mod scheduler;

#[cfg(test)]
pub(crate) use initializer::initialize_repository_at;
#[cfg(test)]
pub(super) use initializer::provision_repository;
pub(crate) use initializer::{initialize_repository, verify_repository_cells};
pub(crate) use router::{RepositoryCell, RepositoryCellPeer, RepositoryCellRouter};
pub(crate) use scheduler::{RepositoryCellScheduler, SchedulerStatus};

const REPOSITORY_MIGRATION: &str = include_str!("cells/migrations/0001_repository_identity.sql");
pub(crate) const REPOSITORY_NAMESPACE: NamespaceId = NamespaceId::from_bytes(*b"crab-repository1");
pub(crate) const REPOSITORY_TICK_COMMAND_ID: u32 = 5;
pub(crate) const REPOSITORY_EFFECT_CLAIM_COMMAND_ID: u32 = 6;
pub(crate) const REPOSITORY_EFFECT_LEASE_COMMAND_ID: u32 = 7;
pub(crate) const REPOSITORY_EFFECT_VALIDATE_QUERY_ID: u32 = 5;
const MAX_LIVE_NODES: usize = 10_000;
const MAX_MIGRATION_STATUS_LIMIT: usize = 256;
const MAX_MIGRATION_STATUS_EXAMINED: usize = 1_024;
const MAINTENANCE_DRAIN_TIMEOUT: Duration = Duration::from_secs(125);
const MAINTENANCE_DRAIN_POLL: Duration = Duration::from_secs(1);
const MAINTENANCE_RUNTIME_BYTES: usize = 8 * 1024 * 1024;
const MAINTENANCE_ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const MAINTENANCE_ADVERTISEMENT_EXPIRY_MARGIN_MS: i64 = 1_000;
const MAINTENANCE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const MAINTENANCE_HEARTBEAT_RETRY: Duration = Duration::from_millis(500);
const REPOSITORY_COMMANDS: &[OperationDescriptor] = &[
    operation(1, 80 * 1024, 80 * 1024),
    operation(2, 80 * 1024, 80 * 1024),
    operation(3, 96 * 1024, 80 * 1024),
    operation(4, 80 * 1024, 80 * 1024),
    operation(REPOSITORY_TICK_COMMAND_ID, 8, 5),
    operation(REPOSITORY_EFFECT_CLAIM_COMMAND_ID, 8, 1024 * 1024),
    operation(REPOSITORY_EFFECT_LEASE_COMMAND_ID, 1024 * 1024, 9),
    operation(8, 8 * 1024, 4 * 1024),
    operation(9, 8 * 1024, 4 * 1024),
    operation(10, 16, 1),
    operation(11, 8 * 1024, 8 * 1024),
];
const REPOSITORY_QUERIES: &[OperationDescriptor] = &[
    operation(1, 8, 80 * 1024),
    operation(2, 16, 80 * 1024),
    operation(3, 1024, 1024 * 1024),
    operation(4, 32, 1024 * 1024),
    operation(REPOSITORY_EFFECT_VALIDATE_QUERY_ID, 1024 * 1024, 1),
    operation(6, 8, 384 * 1024),
    operation(7, 128, 1024 * 1024),
    operation(8, 64, 8 * 1024),
];

struct RepositoryModule;

impl CellModule for RepositoryModule {
    const NAME: &'static str = "repository";

    fn descriptor(&self) -> &'static ModuleDescriptor {
        repository_descriptor()
    }

    fn register(self, registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
        repository::register(registry)?;
        register_maintenance::<Self>(registry)?;
        register_effect_delivery::<Self>(registry)
    }
}

impl MaintenanceModule for RepositoryModule {
    const MODULE: &'static str = Self::NAME;
    const TICK_COMMAND_ID: u32 = REPOSITORY_TICK_COMMAND_ID;
}

impl EffectModule for RepositoryModule {
    const MODULE: &'static str = Self::NAME;
    const CLAIM_COMMAND_ID: u32 = REPOSITORY_EFFECT_CLAIM_COMMAND_ID;
    const LEASE_COMMAND_ID: u32 = REPOSITORY_EFFECT_LEASE_COMMAND_ID;
    const VALIDATE_QUERY_ID: u32 = REPOSITORY_EFFECT_VALIDATE_QUERY_ID;
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
    image_digest(image)?;
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

pub(crate) async fn bootstrap_release_at(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    image: &str,
) -> Result<Vec<u8>> {
    image_digest(image)?;
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
        verify_current_cells(layout, identity, registry).await?;
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
        verify_current_cells(layout, identity, registry).await?;
        return activating.encode().map_err(Error::from);
    }
    verify_compatible_cells(layout, identity, registry).await?;
    verify_current_cells(layout, identity, registry).await?;
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

#[derive(Serialize)]
struct MigrationStatusPage {
    version: u8,
    operation: String,
    release: String,
    entries: Vec<MigrationStatusEntry>,
    next_after: Option<String>,
    has_more: bool,
}

#[derive(Serialize)]
struct MigrationStatusEntry {
    cell: String,
    namespace: String,
    state: MigrationStatusState,
    code: String,
    schema: u32,
    target_code: String,
    target_schema: u32,
    attempts: u32,
    failure: Option<MigrationFailure>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum MigrationStatusState {
    Pending,
    Failed,
}

pub(crate) async fn release_migrations(
    config: &Config,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<u8>> {
    if limit == 0 || limit > MAX_MIGRATION_STATUS_LIMIT {
        return Err(Error::Config(
            "release migration status limit must be 1..=256",
        ));
    }
    let after = after.map(decode_cell_cursor).transpose()?;
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = identities
        .load()
        .await?
        .ok_or(Error::Config("Cell application is not initialized"))?;
    let layout = identities.layout(identity).await?;
    release_migrations_at(&layout, identity, &compiled_registry()?, after, limit).await
}

async fn release_migrations_at(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    after: Option<CellId>,
    limit: usize,
) -> Result<Vec<u8>> {
    let release = ReleaseStore::new(layout.clone(), identity)?
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not prepared"))?;
    let selected = release
        .record()
        .desired()
        .or(release.record().current())
        .ok_or(Error::Config(
            "Cell application release has no selected descriptor",
        ))?;
    if selected != registry.release_digest() {
        return Err(Error::Config(
            "selected Cell release differs from this binary",
        ));
    }
    if ReleaseStore::new(layout.clone(), identity)?
        .descriptor(selected)
        .await?
        != registry.release_bytes()
    {
        return Err(Error::Config(
            "selected Cell descriptor differs from this binary",
        ));
    }
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let authority = CellAuthority::new(layout.clone());
    let progress = MigrationProgressStore::new(layout.clone(), identity)?;
    let operation = release.record().operation();
    let mut entries = Vec::new();
    let mut examined = 0;
    let mut last_examined = None;
    let mut last_returned = None;
    let start_shard = after.map_or(0, |cell| cell.as_bytes()[0]);
    for shard in start_shard..=u8::MAX {
        let mut scan = catalog.scan_shard(shard).await?;
        while let Some(page) = scan.next_page().await? {
            for proof in page.entries() {
                let cell = proof.entry().cell();
                if after.is_some_and(|after| cell.as_bytes() <= after.as_bytes()) {
                    continue;
                }
                if examined == MAX_MIGRATION_STATUS_EXAMINED {
                    return encode_migration_status(
                        operation,
                        selected,
                        entries,
                        last_examined,
                        true,
                    );
                }
                examined += 1;
                last_examined = Some(cell);
                let control = authority.load(cell).await?;
                if control
                    .as_ref()
                    .is_some_and(|control| control.value().state == ControlState::Tombstoned)
                {
                    continue;
                }
                let (code, schema) = control.as_ref().map_or(
                    (proof.entry().initial_code(), proof.entry().initial_schema()),
                    |control| (control.value().code, control.value().schema),
                );
                if registry.is_current_cell(
                    proof.entry().namespace(),
                    proof.entry().role(),
                    code,
                    schema,
                ) {
                    continue;
                }
                let (target_code, target_schema) = registry
                    .current_cell_version(proof.entry().namespace(), proof.entry().role())
                    .ok_or(Error::Config(
                        "cataloged Cell namespace has no current release version",
                    ))?;
                if entries.len() == limit {
                    return encode_migration_status(
                        operation,
                        selected,
                        entries,
                        last_returned,
                        true,
                    );
                }
                let recorded = progress.load(cell, operation).await?;
                let failed = recorded.as_ref().is_some_and(|recorded| {
                    recorded.attempt().release() == selected
                        && recorded.attempt().to() == (target_code, target_schema)
                        && recorded.state() == MigrationProgressState::Failed
                });
                entries.push(MigrationStatusEntry {
                    cell: status_hex(cell.as_bytes()),
                    namespace: status_hex(proof.entry().namespace().as_bytes()),
                    state: if failed {
                        MigrationStatusState::Failed
                    } else {
                        MigrationStatusState::Pending
                    },
                    code: status_hex(code.as_bytes()),
                    schema,
                    target_code: status_hex(target_code.as_bytes()),
                    target_schema,
                    attempts: recorded.as_ref().map_or(0, |recorded| recorded.attempts()),
                    failure: recorded.and_then(|recorded| recorded.failure()),
                });
                last_returned = Some(cell);
            }
        }
    }
    encode_migration_status(operation, selected, entries, None, false)
}

fn encode_migration_status(
    operation: RequestId,
    release: Digest,
    entries: Vec<MigrationStatusEntry>,
    next_after: Option<CellId>,
    has_more: bool,
) -> Result<Vec<u8>> {
    serde_json::to_vec(&MigrationStatusPage {
        version: 1,
        operation: status_hex(operation.as_bytes()),
        release: status_hex(release.as_bytes()),
        entries,
        next_after: next_after.map(|cell| status_hex(cell.as_bytes())),
        has_more,
    })
    .map_err(Error::from)
}

fn decode_cell_cursor(value: &str) -> Result<CellId> {
    if value.len() != 64 {
        return Err(Error::Config(
            "release migration cursor must be a lowercase Cell ID",
        ));
    }
    let mut bytes = [0; 32];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(Error::Config(
            "release migration cursor must be a lowercase Cell ID",
        ));
    }
    for (output, pair) in bytes.iter_mut().zip(pairs) {
        let high = image_nibble(pair[0]).ok_or(Error::Config(
            "release migration cursor must be a lowercase Cell ID",
        ))?;
        let low = image_nibble(pair[1]).ok_or(Error::Config(
            "release migration cursor must be a lowercase Cell ID",
        ))?;
        *output = (high << 4) | low;
    }
    Ok(CellId::from_bytes(bytes))
}

fn status_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
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
    verify_rolling_predecessor(&releases, observed.record(), registry).await?;
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
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(Error::Config("selected Cell image digest is invalid"));
    }
    for (output, pair) in bytes.iter_mut().zip(pairs) {
        let high =
            image_nibble(pair[0]).ok_or(Error::Config("selected Cell image digest is invalid"))?;
        let low =
            image_nibble(pair[1]).ok_or(Error::Config("selected Cell image digest is invalid"))?;
        *output = (high << 4) | low;
    }
    if bytes == [0; 32] {
        return Err(Error::Config("selected Cell image digest is zero"));
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

pub(crate) async fn activate_release(
    config: &Config,
    expected_revision: u64,
    minimum_eligible_nodes: usize,
) -> Result<Vec<u8>> {
    validate_eligible_node_quorum(minimum_eligible_nodes)?;
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
    verify_rolling_predecessor(&releases, observed.record(), &registry).await?;
    let directory = if observed.record().state() == ReleaseState::Ready {
        None
    } else {
        let peer_tls = crate::peer_tls::LoadedPeerTls::load(&config.cells)?;
        Some(NodeDirectory::new(
            layout.clone(),
            peer_tls.fleet(),
            image_digest(observed.record().desired_image())?,
            registry.release_digest(),
        ))
    };
    if let Some(directory) = &directory {
        verify_eligible_nodes(directory, &registry, unix_now_ms()?, minimum_eligible_nodes).await?;
    }
    let operation = observed.record().operation();
    let activating = releases
        .start_activation(expected_revision, operation)
        .await?;
    if activating.state() == ReleaseState::Ready {
        verify_current_cells(&layout, identity, &registry).await?;
        return activating.encode().map_err(Error::from);
    }
    verify_compatible_cells(&layout, identity, &registry).await?;
    if let Some(directory) = &directory {
        verify_eligible_nodes(directory, &registry, unix_now_ms()?, minimum_eligible_nodes).await?;
    }
    verify_current_cells(&layout, identity, &registry).await?;
    releases
        .complete_activation(activating.revision(), operation)
        .await?
        .encode()
        .map_err(Error::from)
}

pub(crate) async fn enter_maintenance(config: &Config, expected_revision: u64) -> Result<Vec<u8>> {
    let root = StorageRoot::build(&config.storage)?;
    let identities =
        ApplicationIdentityStore::new(root.store.clone(), Path::from(root.prefix.clone()));
    let identity = identities
        .load()
        .await?
        .ok_or(Error::Config("Cell application is not initialized"))?;
    let layout = identities.layout(identity).await?;
    let registry = Arc::new(compiled_registry()?);
    let peer_tls = crate::peer_tls::LoadedPeerTls::load(&config.cells)?;
    let releases = ReleaseStore::new(layout.clone(), identity)?;
    let observed = releases
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not prepared"))?;
    let inspect_persisted_work = match observed.record().current() {
        Some(current) if current != registry.release_digest() => {
            registry.requires_persisted_work_inventory_from(&releases.descriptor(current).await?)?
        }
        _ => false,
    };
    let image = image_digest(observed.record().desired_image())?;
    let directory = NodeDirectory::new(
        layout.clone(),
        peer_tls.fleet(),
        image,
        registry.release_digest(),
    );
    let maintenance = enter_maintenance_at(
        &releases,
        &registry,
        &directory,
        expected_revision,
        MAINTENANCE_DRAIN_TIMEOUT,
    )
    .await?;
    if maintenance.state() == ReleaseState::Ready {
        let lease_session = SessionId::from_bytes(*maintenance.operation().as_bytes());
        if let Some(stale) = directory.load(lease_session, unix_now_ms()?).await? {
            directory.withdraw(&stale, unix_now_ms()?).await?;
        }
        return maintenance.encode().map_err(Error::from);
    }

    std::fs::create_dir_all(&config.cells.data_dir)?;
    let session_dir = tempfile::Builder::new()
        .prefix("maintenance-")
        .tempdir_in(&config.cells.data_dir)?;
    let session = SessionId::from_bytes(Uuid::now_v7().into_bytes());
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1)?,
        MAINTENANCE_RUNTIME_BYTES,
        session,
    )?;
    let router = RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        runtime.clone(),
        RepositoryCellPeer::new(
            directory.clone(),
            Arc::new(PeerSigner::new(
                session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            Arc::new(OfflinePeerRoundTrip),
            Owner {
                session,
                endpoint: config.cells.peer_advertise.to_string(),
            },
        ),
        session_dir.path().to_owned(),
    )?;
    let lease = MaintenanceAdvertisement::new(
        directory.clone(),
        peer_tls.signing_key().clone(),
        SessionId::from_bytes(*maintenance.operation().as_bytes()),
        config.cells.peer_advertise.to_string(),
        peer_tls.fleet(),
        peer_tls.certificate(),
        image,
        registry.release_digest(),
        registry.module_digests(),
        rand::random::<u64>().max(1),
    );
    let advertised = match lease.publish_initial().await {
        Ok(advertised) => advertised,
        Err(error) => {
            runtime.shutdown().await?;
            return Err(error);
        }
    };
    complete_maintenance_inventory(
        &layout,
        identity,
        &releases,
        &registry,
        &directory,
        &router,
        &runtime,
        lease,
        advertised,
        maintenance,
        inspect_persisted_work,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "offline release, fleet and runtime authorities remain explicit"
)]
async fn complete_maintenance_inventory(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    releases: &ReleaseStore,
    registry: &Registry,
    directory: &NodeDirectory,
    router: &RepositoryCellRouter,
    runtime: &CellRuntime,
    lease: MaintenanceAdvertisement,
    advertised: VersionedNodeAdvertisement,
    maintenance: ReleaseRecord,
    inspect_persisted_work: bool,
) -> Result<Vec<u8>> {
    let lease_session = lease.session;
    let lease_shutdown = CancellationToken::new();
    let mut heartbeat = tokio::spawn(lease.run(advertised, lease_shutdown.clone()));
    let migration = migrate_maintenance_inventory(layout, identity, router, inspect_persisted_work);
    tokio::pin!(migration);
    let migrated = tokio::select! {
        result = &mut migration => result,
        joined = &mut heartbeat => {
            let lease_result = match joined {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            };
            let shutdown = runtime.shutdown().await;
            lease_result?;
            shutdown?;
            return Err(Error::Config("Cell maintenance executor stopped unexpectedly"));
        }
    };
    let shutdown = runtime.shutdown().await;
    let completed = async {
        migrated?;
        shutdown?;
        let advertised_sessions = directory
            .advertised_sessions(unix_now_ms()?, MAX_LIVE_NODES)
            .await?;
        if advertised_sessions.as_slice() != [lease_session] {
            return Err(Error::Config(
                "Cell maintenance executor does not exclusively own the node directory",
            ));
        }
        verify_current_cells(layout, identity, registry).await?;
        releases
            .complete_maintenance(maintenance.revision(), maintenance.operation())
            .await?
            .encode()
            .map_err(Error::from)
    }
    .await;
    lease_shutdown.cancel();
    let lease_result = match heartbeat.await {
        Ok(result) => result,
        Err(error) => Err(error.into()),
    };
    match (completed, lease_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(completed), Ok(())) => Ok(completed),
    }
}

async fn enter_maintenance_at(
    releases: &ReleaseStore,
    registry: &Registry,
    directory: &NodeDirectory,
    expected_revision: u64,
    timeout: Duration,
) -> Result<ReleaseRecord> {
    let observed = releases
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not prepared"))?;
    if observed.record().desired() != Some(registry.release_digest()) {
        return Err(Error::Config(
            "prepared Cell release differs from this binary",
        ));
    }
    if releases.descriptor(registry.release_digest()).await? != registry.release_bytes() {
        return Err(Error::Config(
            "prepared Cell descriptor differs from this binary",
        ));
    }
    let operation = observed.record().operation();
    if observed.record().state() == ReleaseState::Ready
        && observed.record().current() == Some(registry.release_digest())
        && expected_revision
            .checked_add(2)
            .is_some_and(|revision| observed.record().revision() == revision)
    {
        return Ok(observed.record().clone());
    }
    let maintenance = releases
        .start_maintenance(expected_revision, operation)
        .await?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        directory.collect_stale(unix_now_ms()?, 128).await?;
        if directory
            .advertised_sessions(unix_now_ms()?, MAX_LIVE_NODES)
            .await?
            .is_empty()
        {
            return Ok(maintenance);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(Error::Config(
                "Cell maintenance still has advertised node sessions",
            ));
        }
        tokio::time::sleep(MAINTENANCE_DRAIN_POLL.min(deadline - now)).await;
    }
}

async fn migrate_maintenance_inventory(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    router: &RepositoryCellRouter,
    inspect_persisted_work: bool,
) -> Result<()> {
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let authority = CellAuthority::new(layout.clone());
    for shard in 0_u8..=u8::MAX {
        let mut scan = catalog.scan_shard(shard).await?;
        while let Some(page) = scan.next_page().await? {
            for proof in page.entries() {
                if authority
                    .load(proof.entry().cell())
                    .await?
                    .is_some_and(|control| control.value().state == ControlState::Tombstoned)
                {
                    continue;
                }
                let target = CellTarget::new(
                    identity.tenant(),
                    identity.application(),
                    proof.entry().namespace(),
                    proof.entry().partition(),
                )?;
                router.migrate_target(target.clone()).await?;
                if inspect_persisted_work
                    && let Some(blocker) =
                        router.persisted_work_target(target).await?.first_blocker()
                {
                    return Err(crab_cell_runtime::Error::Release(blocker).into());
                }
            }
        }
    }
    Ok(())
}

struct MaintenanceAdvertisement {
    directory: NodeDirectory,
    signing_key: SigningKey,
    session: SessionId,
    endpoint: String,
    fleet: Digest,
    certificate: Digest,
    image: Digest,
    release: Digest,
    module_digests: Vec<Digest>,
    progress: u64,
}

impl MaintenanceAdvertisement {
    #[expect(
        clippy::too_many_arguments,
        reason = "the signed maintenance executor identity remains explicit"
    )]
    fn new(
        directory: NodeDirectory,
        signing_key: SigningKey,
        session: SessionId,
        endpoint: String,
        fleet: Digest,
        certificate: Digest,
        image: Digest,
        release: Digest,
        module_digests: Vec<Digest>,
        progress: u64,
    ) -> Self {
        Self {
            directory,
            signing_key,
            session,
            endpoint,
            fleet,
            certificate,
            image,
            release,
            module_digests,
            progress,
        }
    }

    async fn publish_initial(&self) -> Result<VersionedNodeAdvertisement> {
        let now_ms = unix_now_ms()?;
        self.directory
            .create(self.advertisement(now_ms)?, now_ms)
            .await
            .map_err(Into::into)
    }

    async fn run(
        self,
        mut observed: VersionedNodeAdvertisement,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let heartbeat = 'heartbeat: loop {
            tokio::select! {
                () = shutdown.cancelled() => break Ok(()),
                () = tokio::time::sleep(MAINTENANCE_HEARTBEAT_INTERVAL) => {}
            }
            loop {
                let now_ms = match unix_now_ms() {
                    Ok(now_ms) => now_ms,
                    Err(error) => break 'heartbeat Err(error),
                };
                let next = match self.advertisement(now_ms) {
                    Ok(next) => next,
                    Err(error) => break 'heartbeat Err(error),
                };
                match self.directory.refresh(&observed, next, now_ms).await {
                    Ok(next) => {
                        observed = next;
                        break;
                    }
                    Err(error) => {
                        let retry_deadline = observed
                            .advertisement()
                            .expires_at_ms()
                            .saturating_sub(MAINTENANCE_ADVERTISEMENT_EXPIRY_MARGIN_MS);
                        if now_ms >= retry_deadline {
                            break 'heartbeat Err(error.into());
                        }
                        let retry_ms = retry_deadline
                            .saturating_sub(now_ms)
                            .min(MAINTENANCE_HEARTBEAT_RETRY.as_millis() as i64);
                        tokio::select! {
                            () = shutdown.cancelled() => break 'heartbeat Ok(()),
                            () = tokio::time::sleep(Duration::from_millis(retry_ms as u64)) => {}
                        }
                    }
                }
            }
        };
        let withdrawal = match unix_now_ms() {
            Ok(now_ms) => self
                .directory
                .withdraw(&observed, now_ms)
                .await
                .map_err(Error::from),
            Err(error) => Err(error),
        };
        match (heartbeat, withdrawal) {
            (Err(error), Err(withdrawal)) => {
                tracing::warn!(
                    error = %withdrawal,
                    "failed to withdraw maintenance executor advertisement"
                );
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), withdrawal) => withdrawal,
        }
    }

    fn advertisement(&self, now_ms: i64) -> Result<NodeAdvertisement> {
        NodeAdvertisement::sign(
            self.session,
            self.endpoint.clone(),
            self.fleet,
            self.certificate,
            self.image,
            self.release,
            &self.signing_key,
            self.progress,
            now_ms,
            now_ms.saturating_add(MAINTENANCE_ADVERTISEMENT_LIFETIME_MS),
            self.module_digests.clone(),
            vec![1],
            NodeCapacity {
                free_memory_bytes: 0,
                free_disk_bytes: 0,
                job_credits: 0,
            },
        )
        .map_err(Into::into)
    }
}

struct OfflinePeerRoundTrip;

impl PeerRoundTrip for OfflinePeerRoundTrip {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async {
            Err(crab_cell_runtime::Error::PeerTransport {
                context: "offline maintenance cannot contact a peer",
                source: Box::new(std::io::Error::other(
                    "offline maintenance peer transport was invoked",
                )),
            })
        })
    }
}

async fn verify_rolling_predecessor(
    releases: &ReleaseStore,
    release: &crab_cell_runtime::ReleaseRecord,
    registry: &Registry,
) -> Result<()> {
    let Some(current) = release.current() else {
        return Ok(());
    };
    if current == registry.release_digest() {
        return Ok(());
    }
    let predecessor = releases.descriptor(current).await?;
    registry.verify_rolling_from(&predecessor)?;
    Ok(())
}

async fn verify_eligible_nodes(
    directory: &NodeDirectory,
    registry: &Registry,
    now_ms: i64,
    minimum_eligible_nodes: usize,
) -> Result<()> {
    validate_eligible_node_quorum(minimum_eligible_nodes)?;
    let live = directory.live(now_ms, MAX_LIVE_NODES).await?;
    let required_modules = registry.module_digests();
    if live
        .iter()
        .any(|node| node.module_digests() != required_modules)
    {
        return Err(Error::Config(
            "live Cell node does not contain the selected module inventory",
        ));
    }
    if live.len() < minimum_eligible_nodes {
        return Err(Error::Config(
            "Cell release activation requires the configured eligible-node quorum",
        ));
    }
    Ok(())
}

fn validate_eligible_node_quorum(minimum_eligible_nodes: usize) -> Result<()> {
    if !(1..=MAX_LIVE_NODES).contains(&minimum_eligible_nodes) {
        return Err(Error::Config(
            "Cell release eligible-node quorum must be between 1 and 10000",
        ));
    }
    Ok(())
}

pub(crate) fn unix_now_ms() -> Result<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::Config("system clock precedes the Unix epoch"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::Config("system clock exceeds the supported Cell range"))
}

async fn verify_compatible_cells(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
) -> Result<()> {
    verify_cell_inventory(layout, identity, registry, CellInventory::Executable).await
}

async fn verify_current_cells(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
) -> Result<()> {
    verify_cell_inventory(layout, identity, registry, CellInventory::Current).await
}

#[derive(Clone, Copy)]
enum CellInventory {
    Executable,
    Current,
}

impl CellInventory {
    fn accepts(
        self,
        registry: &Registry,
        namespace: NamespaceId,
        role: CatalogRole,
        code: Digest,
        schema: u32,
    ) -> bool {
        match self {
            Self::Executable => registry.supports_cell(namespace, role, code, schema),
            Self::Current => registry.is_current_cell(namespace, role, code, schema),
        }
    }

    const fn error(self) -> &'static str {
        match self {
            Self::Executable => "compiled release cannot execute every cataloged Cell",
            Self::Current => "cataloged Cells still require release migration",
        }
    }
}

async fn verify_cell_inventory(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    requirement: CellInventory,
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
                        Some(control) => requirement.accepts(
                            registry,
                            entry.namespace(),
                            entry.role(),
                            control.value().code,
                            control.value().schema,
                        ),
                        None => requirement.accepts(
                            registry,
                            entry.namespace(),
                            entry.role(),
                            entry.initial_code(),
                            entry.initial_schema(),
                        ),
                    };
                    if !supported {
                        return Err(Error::Config(requirement.error()));
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
        retained_codes: &[],
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
    use std::{
        sync::{Arc, OnceLock},
        time::UNIX_EPOCH,
    };

    use crab_cell_runtime::{
        ApplicationIdentity, BuildDescriptor, CatalogEntry, CellAuthority, CellClient, CellModule,
        CellReplica, CellRuntime, CellTarget, IncarnationId, InvocationError, MigrationDescriptor,
        ModuleDescriptor, MutationIdentity, NamespaceDescriptor, NodeAdvertisement, NodeCapacity,
        Owner, PeerCellResolver, RegistryBuilder, ReplicaLimits, RetainedCodeDescriptor, SessionId,
        SqlWorkerPool,
    };
    use crab_storage::{CellStorageLayout, Store};
    use ed25519_dalek::SigningKey;
    use object_store::memory::InMemory;
    use serde_json::Value;

    use super::repository::{
        CommentKey, CommentPage, CommitStatusCatalog, CreateComment, CreateCommentInput,
        CreateCommentOutcome, CreateCommitStatus, CreateCommitStatusInput,
        CreateCommitStatusOutcome, CreateIssue, CreateIssueInput, CreateIssueOutcome, CreateLabel,
        CreateLabelInput, CreateLabelOutcome, GetComment, GetIssue, IssuePage, LabelCatalog,
        ListComments, ListCommentsInput, ListCommitStatuses, ListIssues, ListIssuesInput,
        ListLabels, RepositoryAuthor, UpdateComment, UpdateCommentInput, UpdateCommentOutcome,
        UpdateIssue, UpdateIssueInput, UpdateIssueOutcome,
    };
    use super::*;

    const ROLLOVER_NAMESPACE: NamespaceId = NamespaceId::from_bytes([31; 16]);
    const ROLLOVER_PREDECESSOR: Digest = Digest::from_bytes([32; 32]);
    const ROLLOVER_SQL: &str = "CREATE TABLE rollover(value BLOB NOT NULL)";

    struct RolloverModule;

    impl CellModule for RolloverModule {
        const NAME: &'static str = "rollover-test";

        fn descriptor(&self) -> &'static ModuleDescriptor {
            static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| ModuleDescriptor {
                name: Self::NAME,
                source_digest: Digest::from_bytes([33; 32]),
                retained_codes: &[RetainedCodeDescriptor {
                    code: ROLLOVER_PREDECESSOR,
                    schema_min: 1,
                    schema_max: 1,
                }],
                schema_min: 1,
                schema_max: 1,
                migrations: Box::leak(Box::new([MigrationDescriptor {
                    version: 1,
                    sql: ROLLOVER_SQL,
                    digest: Digest::from_bytes(*blake3::hash(ROLLOVER_SQL.as_bytes()).as_bytes()),
                }])),
                commands: &[],
                queries: &[],
                workflow_definitions: &[],
                activity_types: &[],
                namespaces: &[NamespaceDescriptor {
                    id: ROLLOVER_NAMESPACE,
                    name: Self::NAME,
                    role: CatalogRole::Sql,
                    shards: 1,
                    effect_targets: &[],
                    dead_letter: None,
                }],
            })
        }

        fn register(self, _registry: &mut RegistryBuilder) -> crab_cell_runtime::Result<()> {
            Ok(())
        }
    }

    fn rollover_registry() -> Registry {
        let mut builder = RegistryBuilder::new(BuildDescriptor {
            source_revision: "rollover-test".into(),
            cargo_lock_digest: Digest::from_bytes([34; 32]),
        });
        builder.register(RolloverModule).unwrap();
        builder.finish().unwrap()
    }

    #[test]
    fn release_image_identity_must_be_a_nonzero_sha256_digest() {
        assert!(image_digest(&format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(image_digest(&format!("sha256:{}", "0".repeat(64))).is_err());
        assert!(image_digest("latest").is_err());
    }

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
            "107973fd0ba63cd83e24180deaa6c51bd4c116cc8eae49a96524ca0b43167045"
        );
        assert_eq!(descriptor["modules"][0]["schema_min"], 1);
        assert_eq!(descriptor["modules"][0]["schema_max"], 1);
        assert_eq!(
            descriptor["modules"][0]["commands"]
                .as_array()
                .unwrap()
                .len(),
            11
        );
        assert_eq!(
            descriptor["modules"][0]["queries"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        assert_eq!(descriptor["namespaces"][0]["role"], "repository");
        assert_eq!(descriptor["namespaces"][0]["shards"], 1);
    }

    #[tokio::test]
    async fn release_activation_requires_configured_live_node_quorum_and_exact_modules() {
        let registry = compiled_registry().unwrap();
        let fleet = Digest::from_bytes([10; 32]);
        let image = Digest::from_bytes([11; 32]);
        let now_ms = 1_000_000;
        let directory = NodeDirectory::new(
            CellStorageLayout::new(
                Store::new(Arc::new(InMemory::new())),
                Path::from("eligible-nodes"),
                [9; 16],
            ),
            fleet,
            image,
            registry.release_digest(),
        );
        assert!(matches!(
            verify_eligible_nodes(&directory, &registry, now_ms, 2).await,
            Err(Error::Config(
                "Cell release activation requires the configured eligible-node quorum"
            ))
        ));

        let key = SigningKey::from_bytes(&[12; 32]);
        directory
            .create(
                NodeAdvertisement::sign(
                    SessionId::from_bytes([13; 16]),
                    "https://node-1.internal:8081".into(),
                    fleet,
                    Digest::from_bytes([14; 32]),
                    image,
                    registry.release_digest(),
                    &key,
                    1,
                    now_ms,
                    now_ms + 10_000,
                    registry.module_digests(),
                    vec![1],
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
        assert!(matches!(
            verify_eligible_nodes(&directory, &registry, now_ms + 1, 2).await,
            Err(Error::Config(
                "Cell release activation requires the configured eligible-node quorum"
            ))
        ));
        directory
            .create(
                NodeAdvertisement::sign(
                    SessionId::from_bytes([15; 16]),
                    "https://node-2.internal:8081".into(),
                    fleet,
                    Digest::from_bytes([16; 32]),
                    image,
                    registry.release_digest(),
                    &key,
                    1,
                    now_ms,
                    now_ms + 10_000,
                    registry.module_digests(),
                    vec![1],
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
        verify_eligible_nodes(&directory, &registry, now_ms + 1, 2)
            .await
            .unwrap();
        assert!(matches!(
            verify_eligible_nodes(&directory, &registry, now_ms + 10_001, 2).await,
            Err(Error::Config(
                "Cell release activation requires the configured eligible-node quorum"
            ))
        ));
        assert!(matches!(
            verify_eligible_nodes(&directory, &registry, now_ms + 1, 0).await,
            Err(Error::Config(
                "Cell release eligible-node quorum must be between 1 and 10000"
            ))
        ));
        assert!(matches!(
            verify_eligible_nodes(&directory, &registry, now_ms + 1, MAX_LIVE_NODES + 1).await,
            Err(Error::Config(
                "Cell release eligible-node quorum must be between 1 and 10000"
            ))
        ));

        let foreign_directory = NodeDirectory::new(
            CellStorageLayout::new(
                Store::new(Arc::new(InMemory::new())),
                Path::from("foreign-node"),
                [9; 16],
            ),
            fleet,
            image,
            registry.release_digest(),
        );
        foreign_directory
            .create(
                NodeAdvertisement::sign(
                    SessionId::from_bytes([17; 16]),
                    "https://foreign-node.internal:8081".into(),
                    fleet,
                    Digest::from_bytes([18; 32]),
                    image,
                    registry.release_digest(),
                    &key,
                    1,
                    now_ms,
                    now_ms + 10_000,
                    vec![Digest::from_bytes([19; 32])],
                    vec![1],
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
        assert!(matches!(
            verify_eligible_nodes(&foreign_directory, &registry, now_ms + 1, 1).await,
            Err(Error::Config(
                "live Cell node does not contain the selected module inventory"
            ))
        ));
    }

    #[tokio::test]
    async fn maintenance_waits_for_expired_sessions_and_resumes_after_withdrawal() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([51; 16]),
            ApplicationId::from_bytes([52; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("maintenance-drain"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let image = Digest::from_bytes([0xaa; 32]);
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let operation = RequestId::from_bytes([53; 16]);
        let prepared = releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                0,
                &format!("sha256:{}", "aa".repeat(32)),
                operation,
            )
            .await
            .unwrap();
        let fleet = Digest::from_bytes([54; 32]);
        let directory = NodeDirectory::new(layout, fleet, image, registry.release_digest());
        let now_ms = unix_now_ms().unwrap();
        let session = directory
            .create(
                NodeAdvertisement::sign(
                    SessionId::from_bytes([55; 16]),
                    "https://node.internal:8789".into(),
                    fleet,
                    Digest::from_bytes([56; 32]),
                    image,
                    registry.release_digest(),
                    &SigningKey::from_bytes(&[57; 32]),
                    1,
                    now_ms - 20_000,
                    now_ms - 10_000,
                    registry.module_digests(),
                    vec![1],
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                    },
                )
                .unwrap(),
                now_ms - 20_000,
            )
            .await
            .unwrap();

        assert!(matches!(
            enter_maintenance_at(
                &releases,
                &registry,
                &directory,
                prepared.revision(),
                Duration::ZERO,
            )
            .await,
            Err(Error::Config(
                "Cell maintenance still has advertised node sessions"
            ))
        ));
        assert_eq!(
            releases.load().await.unwrap().unwrap().record().state(),
            ReleaseState::Maintenance
        );

        directory.withdraw(&session, now_ms).await.unwrap();
        let resumed = enter_maintenance_at(
            &releases,
            &registry,
            &directory,
            prepared.revision(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(resumed.state(), ReleaseState::Maintenance);
    }

    #[tokio::test]
    async fn maintenance_executor_advertisement_is_singleton_per_operation() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([58; 16]),
            ApplicationId::from_bytes([59; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("maintenance-singleton"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let fleet = Digest::from_bytes([60; 32]);
        let image = Digest::from_bytes([61; 32]);
        let directory = NodeDirectory::new(layout, fleet, image, registry.release_digest());
        let session = SessionId::from_bytes([62; 16]);
        let first = MaintenanceAdvertisement::new(
            directory.clone(),
            SigningKey::from_bytes(&[63; 32]),
            session,
            "https://maintenance.internal:8789".into(),
            fleet,
            Digest::from_bytes([64; 32]),
            image,
            registry.release_digest(),
            registry.module_digests(),
            1,
        );
        let observed = first.publish_initial().await.unwrap();
        let contender = MaintenanceAdvertisement::new(
            directory.clone(),
            SigningKey::from_bytes(&[63; 32]),
            session,
            "https://maintenance.internal:8789".into(),
            fleet,
            Digest::from_bytes([64; 32]),
            image,
            registry.release_digest(),
            registry.module_digests(),
            2,
        );

        assert!(contender.publish_initial().await.is_err());
        directory
            .withdraw(&observed, unix_now_ms().unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn maintenance_completion_publishes_ready_after_offline_inventory() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([61; 16]),
            ApplicationId::from_bytes([62; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("maintenance-completion"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(compiled_registry().unwrap());
        let image = Digest::from_bytes([0xbb; 32]);
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let operation = RequestId::from_bytes([63; 16]);
        let prepared = releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                0,
                &format!("sha256:{}", "bb".repeat(32)),
                operation,
            )
            .await
            .unwrap();
        let fleet = Digest::from_bytes([64; 32]);
        let directory = NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
        let maintenance = enter_maintenance_at(
            &releases,
            &registry,
            &directory,
            prepared.revision(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        let session = SessionId::from_bytes([65; 16]);
        let runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 1).unwrap(),
            MAINTENANCE_RUNTIME_BYTES,
            session,
        )
        .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let signing_key = SigningKey::from_bytes(&[66; 32]);
        let router = RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            runtime.clone(),
            RepositoryCellPeer::new(
                directory.clone(),
                Arc::new(PeerSigner::new(
                    session,
                    registry.release_digest(),
                    signing_key.clone(),
                )),
                Arc::new(OfflinePeerRoundTrip),
                Owner {
                    session,
                    endpoint: "https://maintenance.internal:8789".into(),
                },
            ),
            scratch.path().to_owned(),
        )
        .unwrap();
        let lease = MaintenanceAdvertisement::new(
            directory.clone(),
            signing_key,
            SessionId::from_bytes(*operation.as_bytes()),
            "https://maintenance.internal:8789".into(),
            fleet,
            Digest::from_bytes([67; 32]),
            image,
            registry.release_digest(),
            registry.module_digests(),
            2,
        );
        let advertised = lease.publish_initial().await.unwrap();

        let completed = complete_maintenance_inventory(
            &layout,
            identity,
            &releases,
            &registry,
            &directory,
            &router,
            &runtime,
            lease,
            advertised,
            maintenance,
            false,
        )
        .await
        .unwrap();
        let encoded: Value = serde_json::from_slice(&completed).unwrap();
        assert_eq!(encoded["state"], "ready");
        assert_eq!(
            enter_maintenance_at(
                &releases,
                &registry,
                &directory,
                prepared.revision(),
                Duration::ZERO,
            )
            .await
            .unwrap()
            .state(),
            ReleaseState::Ready
        );
    }

    #[tokio::test]
    async fn maintenance_restores_migrates_and_publishes_an_old_cell() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([71; 16]),
            ApplicationId::from_bytes([72; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("maintenance-cell-migration"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(rollover_registry());
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            ROLLOVER_NAMESPACE,
            b"maintenance-cell",
        )
        .unwrap();
        let catalog = CellCatalog::new(layout.clone(), identity.tenant());
        let proof = catalog
            .provision(
                CatalogEntry::new(&target, CatalogRole::Sql, ROLLOVER_PREDECESSOR, 1).unwrap(),
            )
            .await
            .unwrap();
        let authority = CellAuthority::new(layout.clone());
        let first_session = SessionId::from_bytes([73; 16]);
        let first = authority
            .create_initial(
                &proof,
                IncarnationId::from_bytes([74; 16]),
                Owner {
                    session: first_session,
                    endpoint: "https://old.internal:8789".into(),
                },
            )
            .await
            .unwrap();
        let first_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 1).unwrap(),
            MAINTENANCE_RUNTIME_BYTES,
            first_session,
        )
        .unwrap();
        let first_scratch = tempfile::tempdir().unwrap();
        first_runtime
            .bootstrap(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *first.value().incarnation.as_bytes(),
                    ReplicaLimits::default(),
                )
                .unwrap(),
                authority.clone(),
                first,
                first_scratch.path().join("old.sqlite"),
                |transaction| transaction.execute_batch(ROLLOVER_SQL).map_err(Into::into),
            )
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
        first_runtime.shutdown().await.unwrap();

        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let operation = RequestId::from_bytes([75; 16]);
        let prepared = releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                0,
                &format!("sha256:{}", "cc".repeat(32)),
                operation,
            )
            .await
            .unwrap();
        let image = Digest::from_bytes([0xcc; 32]);
        let fleet = Digest::from_bytes([76; 32]);
        let directory = NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
        let maintenance = enter_maintenance_at(
            &releases,
            &registry,
            &directory,
            prepared.revision(),
            Duration::ZERO,
        )
        .await
        .unwrap();
        let session = SessionId::from_bytes([77; 16]);
        let runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 1).unwrap(),
            MAINTENANCE_RUNTIME_BYTES,
            session,
        )
        .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let signing_key = SigningKey::from_bytes(&[78; 32]);
        let router = RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            runtime.clone(),
            RepositoryCellPeer::new(
                directory.clone(),
                Arc::new(PeerSigner::new(
                    session,
                    registry.release_digest(),
                    signing_key.clone(),
                )),
                Arc::new(OfflinePeerRoundTrip),
                Owner {
                    session,
                    endpoint: "https://maintenance.internal:8789".into(),
                },
            ),
            scratch.path().to_owned(),
        )
        .unwrap();
        let lease = MaintenanceAdvertisement::new(
            directory.clone(),
            signing_key,
            SessionId::from_bytes(*operation.as_bytes()),
            "https://maintenance.internal:8789".into(),
            fleet,
            Digest::from_bytes([79; 32]),
            image,
            registry.release_digest(),
            registry.module_digests(),
            2,
        );
        let advertised = lease.publish_initial().await.unwrap();

        let completed = complete_maintenance_inventory(
            &layout,
            identity,
            &releases,
            &registry,
            &directory,
            &router,
            &runtime,
            lease,
            advertised,
            maintenance,
            true,
        )
        .await
        .unwrap();
        let record: Value = serde_json::from_slice(&completed).unwrap();
        assert_eq!(record["state"], "ready");
        let migrated = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(
            migrated.value().code,
            registry.module_code(RolloverModule::NAME).unwrap()
        );
        assert_eq!(migrated.value().schema, 1);
        assert_eq!(migrated.value().state, ControlState::Idle);
        assert_eq!(migrated.value().root.as_ref().unwrap().commit_sequence, 1);
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
    async fn retained_cell_is_executable_but_cannot_complete_release_activation() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([35; 16]),
            ApplicationId::from_bytes([36; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("retained-release-inventory"),
            *identity.application().as_bytes(),
        );
        let registry = rollover_registry();
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            ROLLOVER_NAMESPACE,
            b"retained",
        )
        .unwrap();
        CellCatalog::new(layout.clone(), identity.tenant())
            .provision(
                CatalogEntry::new(&target, CatalogRole::Sql, ROLLOVER_PREDECESSOR, 1).unwrap(),
            )
            .await
            .unwrap();

        verify_compatible_cells(&layout, identity, &registry)
            .await
            .unwrap();
        assert!(matches!(
            verify_current_cells(&layout, identity, &registry).await,
            Err(Error::Config(
                "cataloged Cells still require release migration"
            ))
        ));

        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let operation = RequestId::from_bytes([37; 16]);
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
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap();
        let another = CellTarget::new(
            identity.tenant(),
            identity.application(),
            ROLLOVER_NAMESPACE,
            b"new-retained",
        )
        .unwrap();
        assert!(
            releases
                .provision(
                    &CellCatalog::new(layout, identity.tenant()),
                    &registry,
                    CatalogEntry::new(&another, CatalogRole::Sql, ROLLOVER_PREDECESSOR, 1,)
                        .unwrap(),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn release_migration_status_is_bounded_paginated_and_reports_terminal_failure() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([41; 16]),
            ApplicationId::from_bytes([42; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-migration-status"),
            *identity.application().as_bytes(),
        );
        let registry = rollover_registry();
        let catalog = CellCatalog::new(layout.clone(), identity.tenant());
        let mut cells = Vec::new();
        for partition in [b"status-a".as_slice(), b"status-b".as_slice()] {
            let target = CellTarget::new(
                identity.tenant(),
                identity.application(),
                ROLLOVER_NAMESPACE,
                partition,
            )
            .unwrap();
            catalog
                .provision(
                    CatalogEntry::new(&target, CatalogRole::Sql, ROLLOVER_PREDECESSOR, 1).unwrap(),
                )
                .await
                .unwrap();
            cells.push(target.cell_id());
        }
        cells.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

        let operation = RequestId::from_bytes([43; 16]);
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
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
        releases
            .start_activation(prepared.revision(), operation)
            .await
            .unwrap();

        let target_version = registry
            .current_cell_version(ROLLOVER_NAMESPACE, CatalogRole::Sql)
            .unwrap();
        MigrationProgressStore::new(layout.clone(), identity)
            .unwrap()
            .failed(
                crab_cell_runtime::MigrationProgressAttempt::new(
                    operation,
                    registry.release_digest(),
                    SessionId::from_bytes([44; 16]),
                    cells[0],
                    (ROLLOVER_PREDECESSOR, 1),
                    target_version,
                )
                .unwrap(),
                MigrationFailure::Unavailable,
                10,
            )
            .await
            .unwrap();

        let first: Value = serde_json::from_slice(
            &release_migrations_at(&layout, identity, &registry, None, 1)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first["entries"][0]["cell"], status_hex(cells[0].as_bytes()));
        assert_eq!(first["entries"][0]["state"], "failed");
        assert_eq!(first["entries"][0]["failure"], "unavailable");
        assert_eq!(first["entries"][0]["attempts"], 1);
        assert_eq!(first["has_more"], true);

        let second: Value = serde_json::from_slice(
            &release_migrations_at(&layout, identity, &registry, Some(cells[0]), 1)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            second["entries"][0]["cell"],
            status_hex(cells[1].as_bytes())
        );
        assert_eq!(second["entries"][0]["state"], "pending");
        assert_eq!(second["entries"][0]["attempts"], 0);
        assert_eq!(second["has_more"], false);
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
    async fn candidate_startup_rejects_an_unretained_predecessor_contract() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("release-predecessor-contract"),
            *identity.application().as_bytes(),
        );
        let registry = compiled_registry().unwrap();
        let mut predecessor: Value = serde_json::from_slice(registry.release_bytes()).unwrap();
        predecessor["modules"][0]["code"] = Value::String("ab".repeat(32));
        let predecessor = serde_json::to_vec(&predecessor).unwrap();
        let predecessor_digest = Digest::from_bytes(*blake3::hash(&predecessor).as_bytes());
        let releases = ReleaseStore::new(layout.clone(), identity).unwrap();
        let first_operation = RequestId::from_bytes([7; 16]);
        let prepared = releases
            .prepare(
                &predecessor,
                predecessor_digest,
                0,
                &format!("sha256:{}", "a".repeat(64)),
                first_operation,
            )
            .await
            .unwrap();
        let activating = releases
            .start_activation(prepared.revision(), first_operation)
            .await
            .unwrap();
        let ready = releases
            .complete_activation(activating.revision(), first_operation)
            .await
            .unwrap();
        releases
            .prepare(
                registry.release_bytes(),
                registry.release_digest(),
                ready.revision(),
                &format!("sha256:{}", "b".repeat(64)),
                RequestId::from_bytes([8; 16]),
            )
            .await
            .unwrap();

        assert!(
            verify_startup_release_at(&layout, identity, &registry)
                .await
                .is_err()
        );
        assert_eq!(
            releases.load().await.unwrap().unwrap().record().state(),
            ReleaseState::Prepared
        );
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
                    submission_id: [1; 16],
                    author: author.clone(),
                    title: "Durable issue".into(),
                    body: "published through LTX".into(),
                },
            )
            .await
            .unwrap();
        let CreateIssueOutcome::Created(issue_record) = &issue.output else {
            panic!("successful issue command returned a rejection outcome");
        };
        assert_eq!(issue_record.number, 1);
        assert_eq!(issue.receipt.commit_sequence, 1);
        assert_eq!(
            first_client
                .command::<CreateIssue>(
                    &target,
                    issue_identity,
                    CreateIssueInput {
                        submission_id: [1; 16],
                        author: author.clone(),
                        title: "Durable issue".into(),
                        body: "published through LTX".into(),
                    },
                )
                .await
                .unwrap(),
            issue
        );
        let domain_replay = first_client
            .command::<CreateIssue>(
                &target,
                mutation(12),
                CreateIssueInput {
                    submission_id: [1; 16],
                    author: author.clone(),
                    title: "Durable issue".into(),
                    body: "published through LTX".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(domain_replay.output, issue.output);
        assert_eq!(domain_replay.receipt.commit_sequence, 2);
        let submission_conflict = first_client
            .command::<CreateIssue>(
                &target,
                mutation(13),
                CreateIssueInput {
                    submission_id: [1; 16],
                    author: author.clone(),
                    title: "Different issue".into(),
                    body: "published through LTX".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            submission_conflict,
            InvocationError::Rejected(ref outcome)
                if outcome.output == CreateIssueOutcome::RequestConflict
                    && outcome.receipt.commit_sequence == 3
        ));

        let missing = first_client
            .command::<CreateComment>(
                &target,
                mutation(7),
                CreateCommentInput {
                    submission_id: [2; 16],
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
                    && outcome.receipt.commit_sequence == 4
        ));

        let comment = first_client
            .command::<CreateComment>(
                &target,
                mutation(8),
                CreateCommentInput {
                    submission_id: [3; 16],
                    issue: issue_record.number,
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
        assert_eq!(comment.receipt.commit_sequence, 5);

        let forbidden = first_client
            .command::<UpdateIssue>(
                &target,
                mutation(9),
                UpdateIssueInput {
                    number: issue_record.number,
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
                    && outcome.receipt.commit_sequence == 6
        ));

        let invalid_label = first_client
            .command::<UpdateIssue>(
                &target,
                mutation(16),
                UpdateIssueInput {
                    number: issue_record.number,
                    actor: author.clone(),
                    can_manage_metadata: true,
                    version: issue_record.version,
                    title: None,
                    body: None,
                    state: None,
                    label_ids: Some(vec![5]),
                    assignee_subjects: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            invalid_label,
            InvocationError::Rejected(ref outcome)
                if outcome.output == UpdateIssueOutcome::LabelInvalid
                    && outcome.receipt.commit_sequence == 7
        ));

        let first_label = first_client
            .command::<CreateLabel>(
                &target,
                mutation(14),
                CreateLabelInput {
                    submission_id: [14; 16],
                    author: author.clone(),
                    name: "bug".into(),
                    color: "d73a4a".into(),
                    description: None,
                },
            )
            .await
            .unwrap();
        let CreateLabelOutcome::Created(first_label_record) = &first_label.output else {
            panic!("successful label command returned a rejection outcome");
        };
        assert_eq!(first_label.receipt.commit_sequence, 8);
        let second_label = first_client
            .command::<CreateLabel>(
                &target,
                mutation(15),
                CreateLabelInput {
                    submission_id: [15; 16],
                    author: author.clone(),
                    name: "enhancement".into(),
                    color: "84b6eb".into(),
                    description: Some("New capability".into()),
                },
            )
            .await
            .unwrap();
        let CreateLabelOutcome::Created(second_label_record) = &second_label.output else {
            panic!("successful label command returned a rejection outcome");
        };
        assert_eq!(second_label.receipt.commit_sequence, 9);

        let updated_issue = first_client
            .command::<UpdateIssue>(
                &target,
                mutation(10),
                UpdateIssueInput {
                    number: issue_record.number,
                    actor: author.clone(),
                    can_manage_metadata: true,
                    version: issue_record.version,
                    title: Some("Durable issue updated".into()),
                    body: None,
                    state: Some(1),
                    label_ids: Some(vec![first_label_record.number, second_label_record.number]),
                    assignee_subjects: Some(vec!["user-1".into()]),
                },
            )
            .await
            .unwrap();
        let UpdateIssueOutcome::Updated(updated_issue_record) = &updated_issue.output else {
            panic!("successful issue update returned a rejection outcome");
        };
        assert_eq!(updated_issue_record.version, 2);
        assert_eq!(updated_issue_record.label_ids, [1, 2]);
        assert_eq!(updated_issue.receipt.commit_sequence, 10);

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
        assert_eq!(updated_comment.receipt.commit_sequence, 11);

        let status_input = CreateCommitStatusInput {
            submission_id: [17; 16],
            author: updated_comment_record.author.clone(),
            oid: "0123456789abcdef0123456789abcdef01234567".into(),
            context: "ci/test".into(),
            state: 2,
            description: Some("Tests are running".into()),
            target_url: Some("https://ci.example.test/build/17".into()),
        };
        let first_status = first_client
            .command::<CreateCommitStatus>(&target, mutation(17), status_input.clone())
            .await
            .unwrap();
        let CreateCommitStatusOutcome::Created(first_status_record) = &first_status.output else {
            panic!("successful status command returned a rejection outcome");
        };
        assert_eq!(first_status_record.number, 1);
        let second_status = first_client
            .command::<CreateCommitStatus>(
                &target,
                mutation(18),
                CreateCommitStatusInput {
                    submission_id: [18; 16],
                    author: status_input.author.clone(),
                    oid: status_input.oid.clone(),
                    context: "CI/Test".into(),
                    state: 3,
                    description: Some("Tests passed".into()),
                    target_url: Some("https://ci.example.test/build/18".into()),
                },
            )
            .await
            .unwrap();
        let CreateCommitStatusOutcome::Created(second_status_record) = &second_status.output else {
            panic!("second status command returned a rejection outcome");
        };
        assert_eq!(second_status_record.number, 2);
        let status_conflict = first_client
            .command::<CreateCommitStatus>(
                &target,
                mutation(19),
                CreateCommitStatusInput {
                    state: 1,
                    ..status_input.clone()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            status_conflict,
            InvocationError::Rejected(ref outcome)
                if outcome.output == CreateCommitStatusOutcome::RequestConflict
        ));

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
                .query::<ListCommitStatuses>(
                    &target,
                    Some(second_status.receipt),
                    status_input.oid.clone(),
                )
                .await
                .unwrap()
                .output,
            CommitStatusCatalog {
                statuses: vec![second_status_record.as_ref().clone()],
            }
        );
        assert_eq!(
            first_client
                .query::<ListLabels>(&target, Some(updated_comment.receipt), ())
                .await
                .unwrap()
                .output,
            LabelCatalog {
                labels: vec![first_label_record.clone(), second_label_record.clone()],
            }
        );
        assert_eq!(
            first_client
                .query::<ListComments>(
                    &target,
                    Some(updated_comment.receipt),
                    ListCommentsInput {
                        issue: issue_record.number,
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
                .query::<GetIssue>(&target, Some(comment.receipt), issue_record.number)
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
                .query::<ListLabels>(&target, Some(updated_comment.receipt), ())
                .await
                .unwrap()
                .output,
            LabelCatalog {
                labels: vec![first_label_record.clone(), second_label_record.clone()],
            }
        );
        assert_eq!(
            second_client
                .query::<ListCommitStatuses>(
                    &target,
                    Some(second_status.receipt),
                    status_input.oid,
                )
                .await
                .unwrap()
                .output,
            CommitStatusCatalog {
                statuses: vec![second_status_record.as_ref().clone()],
            }
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
