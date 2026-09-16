use std::collections::BTreeSet;

use crab_storage::{CellStorageLayout, StorageError};
use object_store::path::Path;

use super::{
    BackupPin, BackupPinStore, MAX_PAGE_BYTES, MAX_SHARD_BYTES, PinnedCatalogShard, decode_page,
    decode_shard,
};
use crate::{
    ApplicationIdentityStore, CellAuthority, CellCatalog, Control, ControlState, Digest, Error,
    ReleaseRecord, ReleaseState, ReleaseStore, Result,
};

/// Verified result of installing one pin into a separate storage prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupRestore {
    application: crate::ApplicationId,
    pin: crate::RequestId,
    control_count: u64,
    immutable_object_count: u64,
    nonempty_catalog_shards: u16,
}

impl BackupRestore {
    #[must_use]
    pub const fn application(&self) -> crate::ApplicationId {
        self.application
    }

    #[must_use]
    pub const fn pin(&self) -> crate::RequestId {
        self.pin
    }

    #[must_use]
    pub const fn control_count(&self) -> u64 {
        self.control_count
    }

    #[must_use]
    pub const fn immutable_object_count(&self) -> u64 {
        self.immutable_object_count
    }

    #[must_use]
    pub const fn nonempty_catalog_shards(&self) -> u16 {
        self.nonempty_catalog_shards
    }
}

pub(super) struct BackupManifest {
    release: ReleaseRecord,
    release_descriptors: Vec<Digest>,
    catalog: Vec<PinnedCatalogShard>,
    pub(super) controls: Vec<Control>,
    pin_objects: BTreeSet<[u8; 32]>,
}

impl BackupPinStore {
    /// Installs one verified pin into a separate root on the same object store.
    ///
    /// Immutable objects are copied conditionally before catalog, control, and
    /// release pointers become visible. Repeating an interrupted restore adopts
    /// only byte-identical state. Existing divergent state fails closed.
    pub async fn restore(&self, pin: &BackupPin, destination_root: Path) -> Result<BackupRestore> {
        if pin.application != self.identity.application() {
            return Err(Error::Backup("pin belongs to another application"));
        }
        if CellStorageLayout::root_identity_path(&destination_root) == self.layout.identity_path() {
            return Err(Error::Backup(
                "restore destination must be an isolated prefix",
            ));
        }
        let manifest = self.manifest(pin).await?;
        if manifest.release.state() != ReleaseState::Ready {
            return Err(Error::Backup("backup release is not ready"));
        }
        let mut roots = Vec::new();
        for control in &manifest.controls {
            if control.state != ControlState::Tombstoned && control.root.is_none() {
                return Err(Error::Backup("backup contains an unpublished Cell"));
            }
            if let Some(root) = control.ltx_root() {
                let replica = crab_ltx::CellReplica::new(
                    self.layout.clone(),
                    *control.cell.as_bytes(),
                    *control.incarnation.as_bytes(),
                    self.limits,
                )?
                .with_host(self.host.clone());
                roots.push((
                    control.cell,
                    control.incarnation,
                    root,
                    replica.reachable_objects(&root).await?,
                ));
            }
        }

        let identities =
            ApplicationIdentityStore::new(self.layout.store().clone(), destination_root);
        identities.initialize(self.identity).await?;
        let destination = identities.layout(self.identity).await?;
        if destination.immutable_cache_identity() != self.layout.immutable_cache_identity() {
            return Err(Error::Backup(
                "restore destination must use the source object store",
            ));
        }

        for digest in &manifest.release_descriptors {
            self.copy_if_absent(
                self.layout.release_descriptor_path(digest.as_bytes()),
                destination.release_descriptor_path(digest.as_bytes()),
            )
            .await?;
        }
        for shard in &manifest.catalog {
            for digest in &shard.pages {
                self.copy_if_absent(
                    self.layout.catalog_object_path(digest.as_bytes()),
                    destination.catalog_object_path(digest.as_bytes()),
                )
                .await?;
            }
        }
        for digest in &manifest.pin_objects {
            self.copy_if_absent(
                self.layout.pin_object_path(digest),
                destination.pin_object_path(digest),
            )
            .await?;
        }
        let mut immutable_object_count = manifest
            .release_descriptors
            .len()
            .checked_add(manifest.catalog.iter().map(|shard| shard.pages.len()).sum())
            .and_then(|count| count.checked_add(manifest.pin_objects.len()))
            .ok_or(Error::Backup("restore object count overflow"))?;
        for (cell, incarnation, _, objects) in &roots {
            immutable_object_count = immutable_object_count
                .checked_add(objects.len())
                .ok_or(Error::Backup("restore object count overflow"))?;
            for object in objects {
                self.copy_if_absent(
                    self.layout.incarnation_object_path(
                        cell.as_bytes(),
                        incarnation.as_bytes(),
                        &object.digest,
                        object.kind,
                    ),
                    destination.incarnation_object_path(
                        cell.as_bytes(),
                        incarnation.as_bytes(),
                        &object.digest,
                        object.kind,
                    ),
                )
                .await?;
            }
        }

        let destination_releases = ReleaseStore::new(destination.clone(), self.identity)?;
        for digest in &manifest.release_descriptors {
            destination_releases.descriptor(*digest).await?;
        }
        let destination_catalog = CellCatalog::new(destination.clone(), self.identity.tenant());
        for shard in &manifest.catalog {
            destination_catalog
                .pinned_cells(shard.shard, shard.revision, &shard.pages)
                .await?;
        }
        for (cell, incarnation, root, _) in &roots {
            crab_ltx::CellReplica::new(
                destination.clone(),
                *cell.as_bytes(),
                *incarnation.as_bytes(),
                self.limits,
            )?
            .with_host(self.host.clone())
            .reachable_objects(root)
            .await?;
        }

        let destination_authority = CellAuthority::new(destination.clone());
        for control in &manifest.controls {
            let mut restored = control.clone();
            restored.owner = None;
            if restored.state != ControlState::Tombstoned {
                restored.state = ControlState::Idle;
            }
            destination_authority.install_restored(restored).await?;
        }
        for shard in &manifest.catalog {
            destination_catalog
                .install_pinned_shard(shard.shard, shard.revision, &shard.pages)
                .await?;
        }
        destination_releases
            .install_restored(manifest.release.clone())
            .await?;
        self.copy_if_absent(
            self.layout.pin_path(pin.id.as_bytes()),
            destination.pin_path(pin.id.as_bytes()),
        )
        .await?;

        let restored_pins = BackupPinStore::new(
            destination.clone(),
            self.identity,
            self.limits,
            self.host.clone(),
        )?;
        let restored_pin = restored_pins
            .load(pin.id)
            .await?
            .ok_or(Error::Backup("restored pin pointer is absent"))?;
        if restored_pin != *pin {
            return Err(Error::Backup("restored pin pointer differs"));
        }
        restored_pins.verify(&restored_pin).await?;
        for control in &manifest.controls {
            let current = destination_authority
                .load(control.cell)
                .await?
                .ok_or(Error::Backup("restored control is absent"))?;
            if current.value().root != control.root
                || current.value().code != control.code
                || current.value().schema != control.schema
                || current.value().next_due_ms != control.next_due_ms
                || current.value().owner.is_some()
                || (current.value().state != ControlState::Idle
                    && current.value().state != ControlState::Tombstoned)
            {
                return Err(Error::Backup("restored control differs from its pin"));
            }
        }

        Ok(BackupRestore {
            application: self.identity.application(),
            pin: pin.id,
            control_count: pin.control_count,
            immutable_object_count: immutable_object_count as u64,
            nonempty_catalog_shards: manifest
                .catalog
                .iter()
                .filter(|shard| shard.revision != 0)
                .count() as u16,
        })
    }

    pub(super) async fn manifest(&self, pin: &BackupPin) -> Result<BackupManifest> {
        if pin.application != self.identity.application() {
            return Err(Error::Backup("pin belongs to another application"));
        }
        let (release, release_descriptors) = self.load_release(pin.release).await?;
        let catalog_store = CellCatalog::new(self.layout.clone(), self.identity.tenant());
        let mut catalog = pin
            .catalog_revisions
            .iter()
            .enumerate()
            .map(|(shard, revision)| PinnedCatalogShard {
                shard: shard as u8,
                revision: *revision,
                pages: Vec::new(),
            })
            .collect::<Vec<_>>();
        let mut catalog_cells = Vec::new();
        let mut controls = Vec::new();
        let mut pin_objects = BTreeSet::from([pin.release]);
        for shard in &pin.shards {
            pin_objects.insert(shard.digest);
            let body = self.read_object(&shard.digest, MAX_SHARD_BYTES).await?;
            let manifest = decode_shard(&body, shard.shard)?;
            if manifest.catalog_revision != pin.catalog_revisions[usize::from(shard.shard)] {
                return Err(Error::Backup("pin catalog revision differs from its shard"));
            }
            let catalog_pages = manifest
                .catalog_pages
                .iter()
                .copied()
                .map(Digest::from_bytes)
                .collect::<Vec<_>>();
            catalog[usize::from(shard.shard)].pages = catalog_pages.clone();
            catalog_cells.extend(
                catalog_store
                    .pinned_cells(shard.shard, manifest.catalog_revision, &catalog_pages)
                    .await?,
            );
            let before = controls.len();
            for digest in manifest.control_pages {
                pin_objects.insert(digest);
                let body = self.read_object(&digest, MAX_PAGE_BYTES).await?;
                let page = decode_page(&body)?;
                for encoded in page {
                    let control = Control::decode(&super::unhex(&encoded)?)?;
                    if control.cell.as_bytes()[0] != shard.shard {
                        return Err(Error::Backup("pin page crossed a catalog shard"));
                    }
                    controls.push(control);
                }
            }
            if controls.len() - before != manifest.control_count as usize {
                return Err(Error::Backup("pin shard control count differs"));
            }
        }
        if controls.len() as u64 != pin.control_count
            || controls
                .windows(2)
                .any(|pair| pair[0].cell.as_bytes() >= pair[1].cell.as_bytes())
        {
            return Err(Error::Backup("pin controls are not globally ordered"));
        }
        if controls
            .iter()
            .map(|control| control.cell)
            .ne(catalog_cells.iter().copied())
        {
            return Err(Error::Backup("pin controls differ from the pinned catalog"));
        }
        Ok(BackupManifest {
            release,
            release_descriptors,
            catalog,
            controls,
            pin_objects,
        })
    }

    async fn copy_if_absent(&self, source: Path, destination: Path) -> Result<()> {
        match self
            .layout
            .store()
            .copy_if_not_exists(&source, &destination)
            .await
        {
            Ok(()) | Err(StorageError::StateConflict { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
