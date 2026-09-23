use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};

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

    pub(crate) async fn install_restored(&self, control: Control) -> Result<VersionedControl> {
        if control.owner.is_some()
            || !matches!(
                control.state,
                crate::ControlState::Idle | crate::ControlState::Tombstoned
            )
            || (control.state == crate::ControlState::Idle && control.root.is_none())
        {
            return Err(Error::Control("restored control is not safely unowned"));
        }
        let path = self.layout.control_path(control.cell.as_bytes());
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(control.encode()?))
            .await
        {
            Ok(token) => Ok(VersionedControl {
                value: control,
                token,
            }),
            Err(create_error) => match self.load(control.cell).await? {
                Some(current) if current.value == control => Ok(current),
                Some(_) => Err(Error::Control(
                    "restored control conflicts with existing authority",
                )),
                None => Err(create_error.into()),
            },
        }
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
