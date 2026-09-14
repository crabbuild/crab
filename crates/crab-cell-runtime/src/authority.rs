use bytes::Bytes;
use crab_storage::{CellStorageLayout, ETag, StorageError};

use crate::{CatalogProof, CellId, Control, Error, IncarnationId, Owner, Result, Transition};

const MAX_CONTROL_BYTES: u64 = 8 * 1024;

/// One exact control observation and its conditional-write token.
#[derive(Clone)]
pub struct VersionedControl {
    value: Control,
    token: ETag,
}

impl VersionedControl {
    #[must_use]
    pub const fn value(&self) -> &Control {
        &self.value
    }
}

/// Loads and conditionally changes Cell owner/root records.
///
/// Catalog provisioning and takeover timing remain separate policy. Every write
/// must name the observed record and a validated transition; there is no blind
/// overwrite API.
#[derive(Clone)]
pub struct CellAuthority {
    layout: CellStorageLayout,
}

impl CellAuthority {
    #[must_use]
    pub fn new(layout: CellStorageLayout) -> Self {
        Self { layout }
    }

    /// Strict-creates a bootstrap control only after catalog publication.
    pub async fn create_initial(
        &self,
        catalog: &CatalogProof,
        incarnation: IncarnationId,
        owner: Owner,
    ) -> Result<VersionedControl> {
        let entry = catalog.entry();
        let initial = Control::initial(
            entry.cell(),
            incarnation,
            owner,
            entry.initial_code(),
            entry.initial_schema(),
        )?;
        let path = self.layout.control_path(entry.cell().as_bytes());
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(initial.encode()?))
            .await
        {
            Ok(token) => Ok(VersionedControl {
                value: initial,
                token,
            }),
            Err(create_error) => match self.load(entry.cell()).await? {
                Some(current) if current.value == initial => Ok(current),
                Some(_) => Err(Error::CellAlreadyActive),
                None => Err(create_error.into()),
            },
        }
    }

    /// Reads one exact control object; absence is not inferred from listing.
    pub async fn load(&self, cell: CellId) -> Result<Option<VersionedControl>> {
        let path = self.layout.control_path(cell.as_bytes());
        let (body, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_CONTROL_BYTES)
            .await
        {
            Ok(observed) => observed,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value = Control::decode(&body)?;
        if value.cell != cell {
            return Err(crate::Error::Control("control path does not match Cell ID"));
        }
        Ok(Some(VersionedControl { value, token }))
    }

    /// Applies one complete successor with the exact observed ETag.
    ///
    /// A storage conflict is returned to the coordinator for reload and full
    /// predicate revalidation. It is never retried here with a stale successor.
    pub async fn transition(
        &self,
        observed: &VersionedControl,
        next: Control,
        transition: Transition,
    ) -> Result<VersionedControl> {
        observed.value.validate_transition(&next, transition)?;
        let path = self.layout.control_path(observed.value.cell.as_bytes());
        let token = self
            .layout
            .store()
            .update(&path, Bytes::from(next.encode()?), observed.token.clone())
            .await?;
        Ok(VersionedControl { value: next, token })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_storage::Store;
    use object_store::{memory::InMemory, path::Path};

    use super::*;
    use crate::{ControlState, Digest, IncarnationId, Owner, RootRef, SessionId};

    fn control() -> Control {
        Control::initial(
            CellId::from_bytes([1; 32]),
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([3; 16]),
                endpoint: "https://node.internal:8081".into(),
            },
            Digest::from_bytes([4; 32]),
            1,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn stale_control_token_cannot_overwrite_winning_publication() {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = CellStorageLayout::new(store, Path::from("root"), [8; 16]);
        let authority = CellAuthority::new(layout.clone());
        let initial = control();
        layout
            .store()
            .create_strict(
                &layout.control_path(initial.cell.as_bytes()),
                Bytes::from(initial.encode().unwrap()),
            )
            .await
            .unwrap();
        let first = authority.load(initial.cell).await.unwrap().unwrap();
        let stale = authority.load(initial.cell).await.unwrap().unwrap();

        let mut published = initial.clone();
        published.revision += 1;
        published.progress += 1;
        published.state = ControlState::Serving;
        published.root = Some(RootRef {
            digest: Digest::from_bytes([9; 32]),
            txid: 1,
            checksum: (1 << 63) | 7,
            commit_sequence: 1,
        });
        let winner = authority
            .transition(&first, published.clone(), Transition::Publish)
            .await
            .unwrap();
        assert_eq!(winner.value(), &published);

        let mut stale_publish = initial;
        stale_publish.revision += 1;
        stale_publish.progress += 1;
        stale_publish.state = ControlState::Serving;
        stale_publish.root = Some(RootRef {
            digest: Digest::from_bytes([5; 32]),
            txid: 1,
            checksum: (1 << 63) | 8,
            commit_sequence: 1,
        });
        assert!(
            authority
                .transition(&stale, stale_publish, Transition::Publish)
                .await
                .is_err()
        );
        assert_eq!(
            authority
                .load(published.cell)
                .await
                .unwrap()
                .unwrap()
                .value(),
            &published
        );
    }
}
