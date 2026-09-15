use object_store::path::Path;

use crate::Store;

/// Typed physical paths for one application's SQLite Cell objects.
#[derive(Clone)]
pub struct CellStorageLayout {
    store: Store,
    root: Path,
    application: [u8; 16],
}

/// Immutable object kinds accepted below one Cell incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
                .incarnation_object_path(&[0xcd; 32], &[0xef; 16], &[1; 32], CellObjectKind::Root,)
                .as_ref(),
            "tenant-root/cells/v1/apps/abababababababababababababababab/cells/cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd/inc/efefefefefefefefefefefefefefefef/objects/0101010101010101010101010101010101010101010101010101010101010101.root"
        );
    }
}
