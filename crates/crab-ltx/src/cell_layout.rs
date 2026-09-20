use object_store::path::Path;

use crab_storage::Store;

/// Typed physical paths for one application's SQLite Cell objects.
#[derive(Clone)]
pub struct CellStorageLayout {
    store: Store,
    root: Path,
    application: [u8; 16],
}

/// Immutable object kinds accepted below one Cell incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CellObjectKind {
    Ltx,
    Index,
    Directory,
    Root,
    Bundle,
}

impl CellObjectKind {
    const fn extension(self) -> &'static str {
        match self {
            Self::Ltx => "ltx",
            Self::Index => "index",
            Self::Directory => "dir",
            Self::Root => "root",
            Self::Bundle => "bundle",
        }
    }
}

impl CellStorageLayout {
    /// Binds Cell paths to an already validated authoritative storage prefix.
    #[must_use]
    pub fn new(store: Store, root: Path, application: [u8; 16]) -> Self {
        Self {
            store,
            root,
            application,
        }
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    #[must_use]
    pub const fn application_id(&self) -> &[u8; 16] {
        &self.application
    }

    /// Returns the process-local identity used to isolate immutable read caches.
    #[must_use]
    pub fn immutable_cache_identity(&self) -> u64 {
        self.store.immutable_cache_identity()
    }

    #[must_use]
    pub fn identity_path(&self) -> Path {
        Self::root_identity_path(&self.root)
    }

    /// Returns the application identity path before an application ID is known.
    #[must_use]
    pub fn root_identity_path(root: &Path) -> Path {
        Path::from(format!("{root}/cells/v1/identity.json"))
    }

    #[must_use]
    pub fn release_path(&self) -> Path {
        self.application_path("release.json")
    }

    #[must_use]
    pub fn application_prefix(&self) -> Path {
        self.application_path("")
    }

    #[must_use]
    pub fn pin_prefix(&self) -> Path {
        self.application_path("pins")
    }

    #[must_use]
    pub fn release_descriptor_path(&self, digest: &[u8; 32]) -> Path {
        self.application_path(&format!("releases/{}.json", hex(digest)))
    }

    #[must_use]
    pub fn control_path(&self, cell: &[u8; 32]) -> Path {
        self.application_path(&format!("cells/{}/control.json", hex(cell)))
    }

    #[must_use]
    pub fn incarnation_object_path(
        &self,
        cell: &[u8; 32],
        incarnation: &[u8; 16],
        digest: &[u8; 32],
        kind: CellObjectKind,
    ) -> Path {
        self.application_path(&format!(
            "cells/{}/inc/{}/objects/{}.{}",
            hex(cell),
            hex(incarnation),
            hex(digest),
            kind.extension()
        ))
    }

    /// Returns a private, unreferenced staging key for an immutable Cell object.
    ///
    /// Staging keys are never part of a root or manifest. The digest makes
    /// retries and failover converge on one unreferenced target for the same
    /// immutable bytes; callers must promote the object and delete this key
    /// before returning.
    #[must_use]
    pub fn incarnation_staging_path(
        &self,
        cell: &[u8; 32],
        incarnation: &[u8; 16],
        digest: &[u8; 32],
        kind: CellObjectKind,
    ) -> Path {
        self.application_path(&format!(
            "cells/{}/inc/{}/objects/.staging/{}.{}",
            hex(cell),
            hex(incarnation),
            hex(digest),
            kind.extension()
        ))
    }

    #[must_use]
    pub fn catalog_head_path(&self, shard: u8) -> Path {
        self.application_path(&format!("catalog/{shard:02x}/head.json"))
    }

    #[must_use]
    pub fn catalog_object_path(&self, digest: &[u8; 32]) -> Path {
        self.application_path(&format!("catalog/objects/{}.json", hex(digest)))
    }

    #[must_use]
    pub fn pin_path(&self, pin: &[u8; 16]) -> Path {
        self.application_path(&format!("pins/{}.json", hex(pin)))
    }

    #[must_use]
    pub fn pin_object_path(&self, digest: &[u8; 32]) -> Path {
        self.application_path(&format!("pins/objects/{}.json", hex(digest)))
    }

    #[must_use]
    pub fn migration_path(&self, cell: &[u8; 32], operation: &[u8; 16], suffix: &str) -> Path {
        self.application_path(&format!(
            "cells/{}/migration/{}/{}",
            hex(cell),
            hex(operation),
            suffix
        ))
    }

    #[must_use]
    pub fn node_path(&self, session: &[u8; 16]) -> Path {
        Path::from(format!(
            "{}/cells/v1/nodes/{}.json",
            self.root,
            hex(session)
        ))
    }

    #[must_use]
    pub fn node_directory_path(&self) -> Path {
        Path::from(format!("{}/cells/v1/nodes", self.root))
    }

    /// Immutable recovered follower bundle outside any one application prefix.
    #[must_use]
    pub fn node_log_bundle_path(&self, leader: &[u8; 16], epoch: u64, digest: &[u8; 32]) -> Path {
        Path::from(format!(
            "{}/cells/v1/node-logs/{}/{epoch}/bundles/{}.bundle",
            self.root,
            hex(leader),
            hex(digest)
        ))
    }

    /// Content-addressed manifest that pins every Cell tail recovered together.
    #[must_use]
    pub fn node_log_recovery_path(&self, leader: &[u8; 16], epoch: u64, digest: &[u8; 32]) -> Path {
        Path::from(format!(
            "{}/cells/v1/node-logs/{}/{epoch}/recovery/{}.json",
            self.root,
            hex(leader),
            hex(digest)
        ))
    }

    fn application_path(&self, suffix: &str) -> Path {
        Path::from(format!(
            "{}/cells/v1/apps/{}/{}",
            self.root,
            hex(&self.application),
            suffix
        ))
    }
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    #[test]
    fn cell_paths_are_fixed_width_and_scoped_to_application() {
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("tenant-root"),
            [0xab; 16],
        );
        assert_eq!(
            layout.control_path(&[0xcd; 32]).as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/cells/cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/control.json"
        );
        assert_eq!(
            layout.node_directory_path().as_ref(),
            "tenant-root/cells/v1/nodes"
        );
        assert_eq!(
            layout
                .node_log_bundle_path(&[0xdd; 16], 7, &[0xef; 32])
                .as_ref(),
            "tenant-root/cells/v1/node-logs/dddddddddddddddddddddddddddddddd/7/bundles/efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef.bundle"
        );
        assert_eq!(
            layout
                .node_log_recovery_path(&[0xdd; 16], 7, &[0xef; 32])
                .as_ref(),
            "tenant-root/cells/v1/node-logs/dddddddddddddddddddddddddddddddd/7/recovery/efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef.json"
        );
        assert_eq!(
            layout
                .migration_path(&[0xcd; 32], &[0xee; 16], "source.json")
                .as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/cells/cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/migration/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee/source.json"
        );
        assert_eq!(
            layout
                .incarnation_object_path(&[0xcd; 32], &[0xef; 16], &[1; 32], CellObjectKind::Root,)
                .as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/cells/cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/inc/efefefefefefefefefefefefefefefef/objects/0101010101010101010101010101010101010101010101010101010101010101.root"
        );
        assert_eq!(
            layout
                .incarnation_staging_path(
                    &[0xcd; 32],
                    &[0xef; 16],
                    &[7; 32],
                    CellObjectKind::Bundle,
                )
                .as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/cells/cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/inc/efefefefefefefefefefefefefefefef/objects/.staging/0707070707070707070707070707070707070707070707070707070707070707.bundle"
        );
        assert_eq!(
            layout.pin_object_path(&[2; 32]).as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/pins/objects/0202020202020202020202020202020202020202020202020202020202020202.json"
        );
        assert_eq!(
            layout.application_prefix().as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab"
        );
        assert_eq!(
            layout.pin_prefix().as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/pins"
        );
    }

    #[test]
    fn immutable_cache_identity_is_shared_only_by_store_clones() {
        let inner = Arc::new(InMemory::new());
        let first =
            CellStorageLayout::new(Store::new(inner.clone()), Path::from("same-root"), [1; 16]);
        let clone = first.clone();
        let independent =
            CellStorageLayout::new(Store::new(inner), Path::from("same-root"), [1; 16]);
        assert_eq!(
            first.immutable_cache_identity(),
            clone.immutable_cache_identity()
        );
        assert_ne!(
            first.immutable_cache_identity(),
            independent.immutable_cache_identity()
        );
    }
}
