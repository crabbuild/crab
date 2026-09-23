use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{StorageError, Store};
use object_store::path::Path;
use serde::{Deserialize, Serialize};

use crate::identity::encode_hex;
use crate::identity::{ApplicationId, TenantId};
use crate::{Error, Result};

const MAX_IDENTITY_BYTES: u64 = 1_024;

/// Stable tenant and application IDs persisted once for an authoritative root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplicationIdentity {
    tenant: TenantId,
    application: ApplicationId,
}

impl ApplicationIdentity {
    #[must_use]
    pub const fn new(tenant: TenantId, application: ApplicationId) -> Self {
        Self {
            tenant,
            application,
        }
    }

    #[must_use]
    pub const fn tenant(&self) -> TenantId {
        self.tenant
    }

    #[must_use]
    pub const fn application(&self) -> ApplicationId {
        self.application
    }

    fn encode(self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(&RawIdentity {
            application: encode_hex(self.application.as_bytes()),
            tenant: encode_hex(self.tenant.as_bytes()),
            version: 1,
        })?)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let raw: RawIdentity = serde_json::from_slice(bytes)?;
        if raw.version != 1 {
            return Err(Error::Identity("unsupported application identity version"));
        }
        let identity = Self::new(
            TenantId::from_bytes(parse_id(&raw.tenant, "tenant")?),
            ApplicationId::from_bytes(parse_id(&raw.application, "application")?),
        );
        identity.validate()?;
        if identity.encode()?.as_slice() != bytes {
            return Err(Error::Identity("application identity is not canonical"));
        }
        Ok(identity)
    }

    fn validate(self) -> Result<()> {
        if self.tenant.as_bytes().iter().all(|byte| *byte == 0)
            || self.application.as_bytes().iter().all(|byte| *byte == 0)
        {
            return Err(Error::Identity(
                "tenant and application IDs must be nonzero",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawIdentity {
    application: String,
    tenant: String,
    version: u8,
}

/// Owns strict creation and loading of one root's immutable application identity.
#[derive(Clone)]
pub struct ApplicationIdentityStore {
    store: Store,
    root: Path,
    path: Path,
}

impl ApplicationIdentityStore {
    #[must_use]
    pub fn new(store: Store, root: Path) -> Self {
        let path = CellStorageLayout::root_identity_path(&root);
        Self { store, root, path }
    }

    /// Loads the persisted identity, returning absence only for a missing object.
    pub async fn load(&self) -> Result<Option<ApplicationIdentity>> {
        let body = match self
            .store
            .get_with_etag_bounded(&self.path, MAX_IDENTITY_BYTES)
            .await
        {
            Ok((body, _)) => body,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(ApplicationIdentity::decode(&body)?))
    }

    /// Strict-creates the proposed identity or adopts the exact concurrent winner.
    pub async fn initialize(&self, proposed: ApplicationIdentity) -> Result<ApplicationIdentity> {
        proposed.validate()?;
        let encoded = proposed.encode()?;
        match self
            .store
            .create_strict(&self.path, Bytes::from(encoded))
            .await
        {
            Ok(()) => Ok(proposed),
            Err(create_error) => match self.load().await? {
                Some(current) if current == proposed => Ok(current),
                Some(_) => Err(Error::Identity(
                    "authoritative root already belongs to another application",
                )),
                None => Err(create_error.into()),
            },
        }
    }

    /// Binds application-scoped paths only after reloading the persisted identity.
    pub async fn layout(&self, identity: ApplicationIdentity) -> Result<CellStorageLayout> {
        if self.load().await? != Some(identity) {
            return Err(Error::Identity(
                "application identity is not authoritative for this root",
            ));
        }
        Ok(CellStorageLayout::new(
            self.store.clone(),
            self.root.clone(),
            *identity.application().as_bytes(),
        ))
    }
}

fn parse_id(value: &str, field: &'static str) -> Result<[u8; 16]> {
    crate::identity::decode_hex(value).map_err(|_| Error::Identity(field))
}
