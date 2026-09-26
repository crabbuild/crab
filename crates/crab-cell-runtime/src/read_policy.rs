//! S3-backed desired reader count; never an owner or replica assignment.

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::identity::{CellId, IncarnationId, decode_hex, encode_hex};
use crate::{Error, Result};

const MAX_POLICY_BYTES: u64 = 512;
/// Highest operator-requested reader count accepted by the policy codec.
pub const MAX_READERS: u16 = 10_000;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u8,
    cell: String,
    incarnation: String,
    desired_readers: u16,
    revision: u64,
}

/// Advisory desired read-replica count for one Cell incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPolicy {
    cell: CellId,
    incarnation: IncarnationId,
    desired_readers: u16,
    revision: u64,
}

impl ReadPolicy {
    fn new(
        cell: CellId,
        incarnation: IncarnationId,
        desired_readers: u16,
        revision: u64,
    ) -> Result<Self> {
        if incarnation.as_bytes().iter().all(|byte| *byte == 0)
            || cell.as_bytes().iter().all(|byte| *byte == 0)
            || desired_readers > MAX_READERS
            || revision == 0
        {
            return Err(Error::Control("invalid Cell read policy"));
        }
        Ok(Self {
            cell,
            incarnation,
            desired_readers,
            revision,
        })
    }

    /// Returns the Cell this policy configures.
    #[must_use]
    pub const fn cell(&self) -> CellId {
        self.cell
    }

    /// Returns the incarnation this policy configures.
    #[must_use]
    pub const fn incarnation(&self) -> IncarnationId {
        self.incarnation
    }

    /// Returns the requested number of distinct non-owner readers.
    #[must_use]
    pub const fn desired_readers(&self) -> u16 {
        self.desired_readers
    }

    /// Returns the policy CAS revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    fn encode(self) -> Result<Vec<u8>> {
        let raw = RawPolicy {
            version: 1,
            cell: encode_hex(self.cell.as_bytes()),
            incarnation: encode_hex(self.incarnation.as_bytes()),
            desired_readers: self.desired_readers,
            revision: self.revision,
        };
        let bytes = serde_json::to_vec(&raw)?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(Error::Control("Cell read policy exceeds byte limit"));
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err(Error::Control("Cell read policy exceeds byte limit"));
        }
        let raw: RawPolicy = serde_json::from_slice(bytes)?;
        if raw.version != 1 {
            return Err(Error::Control("unsupported Cell read policy version"));
        }
        let policy = Self::new(
            CellId::from_bytes(decode_hex(&raw.cell)?),
            IncarnationId::from_bytes(decode_hex(&raw.incarnation)?),
            raw.desired_readers,
            raw.revision,
        )?;
        if policy.encode()? != bytes {
            return Err(Error::Control("Cell read policy is not canonical"));
        }
        Ok(policy)
    }
}

/// A policy and the exact conditional-write token that read it.
pub struct VersionedReadPolicy {
    value: ReadPolicy,
    token: ETag,
}

impl VersionedReadPolicy {
    /// Returns the decoded desired-count policy.
    #[must_use]
    pub const fn value(&self) -> ReadPolicy {
        self.value
    }
}

/// Loads and conditionally updates advisory read-replica targets in S3.
#[derive(Clone)]
pub struct ReadPolicyStore {
    layout: CellStorageLayout,
}

impl ReadPolicyStore {
    /// Binds read policies to the same application root as Cell control.
    #[must_use]
    pub fn new(layout: CellStorageLayout) -> Self {
        Self { layout }
    }

    /// Loads one exact policy; absence means zero desired readers.
    pub async fn load(&self, cell: CellId) -> Result<Option<VersionedReadPolicy>> {
        let path = self.layout.read_policy_path(cell.as_bytes());
        let (bytes, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_POLICY_BYTES)
            .await
        {
            Ok(found) => found,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value = ReadPolicy::decode(&bytes)?;
        if value.cell != cell {
            return Err(Error::Control("Cell read policy path changed Cell"));
        }
        Ok(Some(VersionedReadPolicy { value, token }))
    }

    /// Strict-creates the first target count for this Cell incarnation.
    pub async fn create(
        &self,
        cell: CellId,
        incarnation: IncarnationId,
        desired_readers: u16,
    ) -> Result<VersionedReadPolicy> {
        let value = ReadPolicy::new(cell, incarnation, desired_readers, 1)?;
        let path = self.layout.read_policy_path(cell.as_bytes());
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(value.encode()?))
            .await
        {
            Ok(token) => Ok(VersionedReadPolicy { value, token }),
            Err(error) => match self.load(cell).await? {
                Some(current) if current.value == value => Ok(current),
                Some(_) | None => Err(error.into()),
            },
        }
    }

    /// CASes the next desired count, preserving the observed incarnation.
    pub async fn update(
        &self,
        observed: &VersionedReadPolicy,
        desired_readers: u16,
    ) -> Result<VersionedReadPolicy> {
        self.update_to(observed, observed.value.incarnation, desired_readers)
            .await
    }

    /// Rebinds an observed policy after the caller has authorized a new Cell incarnation.
    ///
    /// The caller must first compare the new incarnation with current Cell
    /// authority. This policy is advisory and cannot authorize a replica read.
    pub async fn replace_incarnation(
        &self,
        observed: &VersionedReadPolicy,
        incarnation: IncarnationId,
        desired_readers: u16,
    ) -> Result<VersionedReadPolicy> {
        if incarnation == observed.value.incarnation {
            return Err(Error::Control(
                "Cell read policy incarnation did not change",
            ));
        }
        self.update_to(observed, incarnation, desired_readers).await
    }

    async fn update_to(
        &self,
        observed: &VersionedReadPolicy,
        incarnation: IncarnationId,
        desired_readers: u16,
    ) -> Result<VersionedReadPolicy> {
        let value = ReadPolicy::new(
            observed.value.cell,
            incarnation,
            desired_readers,
            observed
                .value
                .revision
                .checked_add(1)
                .ok_or(Error::Control("Cell read policy revision overflow"))?,
        )?;
        let path = self.layout.read_policy_path(value.cell.as_bytes());
        match self
            .layout
            .store()
            .update(&path, Bytes::from(value.encode()?), observed.token.clone())
            .await
        {
            Ok(token) => Ok(VersionedReadPolicy { value, token }),
            Err(error) => match self.load(value.cell).await? {
                Some(current) if current.value == value => Ok(current),
                Some(_) | None => Err(error.into()),
            },
        }
    }
}
