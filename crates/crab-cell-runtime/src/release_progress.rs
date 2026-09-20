use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};

use crate::{
    ApplicationIdentity, CellId, Digest, Error, RequestId, Result, SessionId,
    identity::{decode_hex, encode_hex},
};

const MAX_PROGRESS_BYTES: u64 = 4 * 1024;
const MAX_WRITE_ATTEMPTS: usize = 4;

/// Stable failure class exposed by release migration progress.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationFailure {
    Unavailable,
    Capacity,
    Deadline,
    Incompatible,
    Internal,
}

/// Terminal state of one attempted release migration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationProgressState {
    Completed,
    Failed,
}

/// Immutable scope of one Cell's migration toward an activating release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationProgressAttempt {
    operation: RequestId,
    release: Digest,
    session: SessionId,
    cell: CellId,
    from_code: Digest,
    from_schema: u32,
    to_code: Digest,
    to_schema: u32,
}

impl MigrationProgressAttempt {
    /// Binds progress to one release operation and exact source/target versions.
    pub fn new(
        operation: RequestId,
        release: Digest,
        session: SessionId,
        cell: CellId,
        from: (Digest, u32),
        to: (Digest, u32),
    ) -> Result<Self> {
        let attempt = Self {
            operation,
            release,
            session,
            cell,
            from_code: from.0,
            from_schema: from.1,
            to_code: to.0,
            to_schema: to.1,
        };
        attempt.validate()?;
        Ok(attempt)
    }

    #[must_use]
    pub const fn operation(&self) -> RequestId {
        self.operation
    }

    #[must_use]
    pub const fn release(&self) -> Digest {
        self.release
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn cell(&self) -> CellId {
        self.cell
    }

    #[must_use]
    pub const fn from(&self) -> (Digest, u32) {
        (self.from_code, self.from_schema)
    }

    #[must_use]
    pub const fn to(&self) -> (Digest, u32) {
        (self.to_code, self.to_schema)
    }

    fn validate(&self) -> Result<()> {
        if self.operation.as_bytes().iter().all(|byte| *byte == 0)
            || self.release.as_bytes().iter().all(|byte| *byte == 0)
            || self.session.as_bytes().iter().all(|byte| *byte == 0)
            || self.cell.as_bytes().iter().all(|byte| *byte == 0)
            || self.from_code.as_bytes().iter().all(|byte| *byte == 0)
            || self.to_code.as_bytes().iter().all(|byte| *byte == 0)
            || self.from_schema == 0
            || self.to_schema == 0
            || self.from_code == self.to_code && self.from_schema == self.to_schema
        {
            return Err(Error::Release("invalid release migration progress scope"));
        }
        Ok(())
    }
}

/// Canonical latest terminal result for one release operation and Cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgress {
    application: crate::ApplicationId,
    attempt: MigrationProgressAttempt,
    revision: u64,
    attempts: u32,
    state: MigrationProgressState,
    failure: Option<MigrationFailure>,
    updated_at_ms: i64,
}

impl MigrationProgress {
    #[must_use]
    pub const fn attempt(&self) -> MigrationProgressAttempt {
        self.attempt
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    #[must_use]
    pub const fn state(&self) -> MigrationProgressState {
        self.state
    }

    #[must_use]
    pub const fn failure(&self) -> Option<MigrationFailure> {
        self.failure
    }

    #[must_use]
    pub const fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let raw = RawMigrationProgress {
            application: encode_hex(self.application.as_bytes()),
            attempts: self.attempts,
            cell: encode_hex(self.attempt.cell.as_bytes()),
            failure: self.failure,
            from_code: encode_hex(self.attempt.from_code.as_bytes()),
            from_schema: self.attempt.from_schema,
            operation: encode_hex(self.attempt.operation.as_bytes()),
            release: encode_hex(self.attempt.release.as_bytes()),
            revision: self.revision.to_string(),
            session: encode_hex(self.attempt.session.as_bytes()),
            state: self.state,
            to_code: encode_hex(self.attempt.to_code.as_bytes()),
            to_schema: self.attempt.to_schema,
            updated_at_ms: self.updated_at_ms.to_string(),
            version: 1,
        };
        let encoded = serde_json::to_vec(&raw)?;
        if encoded.len() as u64 > MAX_PROGRESS_BYTES {
            return Err(Error::Release("release migration progress exceeds 4 KiB"));
        }
        Ok(encoded)
    }

    fn decode(bytes: &[u8], identity: ApplicationIdentity) -> Result<Self> {
        let raw: RawMigrationProgress = serde_json::from_slice(bytes)?;
        if raw.version != 1 {
            return Err(Error::Release(
                "unsupported release migration progress version",
            ));
        }
        let progress = Self {
            application: crate::ApplicationId::from_bytes(parse_hex(&raw.application)?),
            attempt: MigrationProgressAttempt {
                operation: RequestId::from_bytes(parse_hex(&raw.operation)?),
                release: Digest::from_bytes(parse_hex(&raw.release)?),
                session: SessionId::from_bytes(parse_hex(&raw.session)?),
                cell: CellId::from_bytes(parse_hex(&raw.cell)?),
                from_code: Digest::from_bytes(parse_hex(&raw.from_code)?),
                from_schema: raw.from_schema,
                to_code: Digest::from_bytes(parse_hex(&raw.to_code)?),
                to_schema: raw.to_schema,
            },
            revision: parse_u64(&raw.revision)?,
            attempts: raw.attempts,
            state: raw.state,
            failure: raw.failure,
            updated_at_ms: parse_i64(&raw.updated_at_ms)?,
        };
        progress.validate(identity)?;
        if progress.encode()?.as_slice() != bytes {
            return Err(Error::Release(
                "release migration progress is not canonical",
            ));
        }
        Ok(progress)
    }

    fn validate(&self, identity: ApplicationIdentity) -> Result<()> {
        self.attempt.validate()?;
        if self.application != identity.application()
            || self.revision == 0
            || self.attempts == 0
            || self.updated_at_ms < 0
            || matches!(self.state, MigrationProgressState::Completed) && self.failure.is_some()
            || matches!(self.state, MigrationProgressState::Failed) && self.failure.is_none()
        {
            return Err(Error::Release("invalid release migration progress"));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMigrationProgress {
    application: String,
    attempts: u32,
    cell: String,
    failure: Option<MigrationFailure>,
    from_code: String,
    from_schema: u32,
    operation: String,
    release: String,
    revision: String,
    session: String,
    state: MigrationProgressState,
    to_code: String,
    to_schema: u32,
    updated_at_ms: String,
    version: u8,
}

struct VersionedProgress {
    progress: MigrationProgress,
    token: ETag,
}

/// CAS-backed terminal progress records for a release migration operation.
#[derive(Clone)]
pub struct MigrationProgressStore {
    layout: CellStorageLayout,
    identity: ApplicationIdentity,
}

impl MigrationProgressStore {
    pub fn new(layout: CellStorageLayout, identity: ApplicationIdentity) -> Result<Self> {
        if layout.application_id() != identity.application().as_bytes() {
            return Err(Error::Release(
                "migration progress layout and application differ",
            ));
        }
        Ok(Self { layout, identity })
    }

    /// Loads and verifies the latest terminal result for one operation and Cell.
    pub async fn load(
        &self,
        cell: CellId,
        operation: RequestId,
    ) -> Result<Option<MigrationProgress>> {
        Ok(self
            .load_versioned(cell, operation)
            .await?
            .map(|versioned| versioned.progress))
    }

    /// Records a migration whose target code/schema is now authoritative.
    pub async fn completed(
        &self,
        attempt: MigrationProgressAttempt,
        now_ms: i64,
    ) -> Result<MigrationProgress> {
        self.record(attempt, MigrationProgressState::Completed, None, now_ms)
            .await
    }

    /// Records a bounded failure class without persisting source error text.
    pub async fn failed(
        &self,
        attempt: MigrationProgressAttempt,
        failure: MigrationFailure,
        now_ms: i64,
    ) -> Result<MigrationProgress> {
        self.record(
            attempt,
            MigrationProgressState::Failed,
            Some(failure),
            now_ms,
        )
        .await
    }

    async fn record(
        &self,
        attempt: MigrationProgressAttempt,
        state: MigrationProgressState,
        failure: Option<MigrationFailure>,
        now_ms: i64,
    ) -> Result<MigrationProgress> {
        attempt.validate()?;
        if now_ms < 0 {
            return Err(Error::Release("negative migration progress time"));
        }
        let mut last_write_error = None;
        for _ in 0..MAX_WRITE_ATTEMPTS {
            let observed = self.load_versioned(attempt.cell, attempt.operation).await?;
            if let Some(observed) = &observed
                && !same_operation(&observed.progress.attempt, &attempt)
            {
                return Err(Error::Release("release migration progress scope changed"));
            }
            if let Some(observed) = &observed
                && observed.progress.state == MigrationProgressState::Completed
            {
                return Ok(observed.progress.clone());
            }
            let next = MigrationProgress {
                application: self.identity.application(),
                attempt,
                revision: observed.as_ref().map_or(Ok(1), |value| {
                    value
                        .progress
                        .revision
                        .checked_add(1)
                        .ok_or(Error::Release("migration progress revision overflow"))
                })?,
                attempts: observed.as_ref().map_or(Ok(1), |value| {
                    value
                        .progress
                        .attempts
                        .checked_add(1)
                        .ok_or(Error::Release("migration progress attempt overflow"))
                })?,
                state,
                failure,
                updated_at_ms: now_ms,
            };
            next.validate(self.identity)?;
            let bytes = Bytes::from(next.encode()?);
            let path = self.path(attempt.cell, attempt.operation);
            let write = match observed {
                Some(observed) => {
                    self.layout
                        .store()
                        .update(&path, bytes, observed.token)
                        .await
                }
                None => {
                    self.layout
                        .store()
                        .create_strict_with_etag(&path, bytes)
                        .await
                }
            };
            match write {
                Ok(_) => return Ok(next),
                Err(error) => {
                    if let Some(current) =
                        self.load_versioned(attempt.cell, attempt.operation).await?
                        && current.progress == next
                    {
                        return Ok(current.progress);
                    }
                    if !retryable_storage_error(&error) {
                        return Err(error.into());
                    }
                    last_write_error = Some(error);
                }
            }
        }
        Err(last_write_error.map_or(
            Error::Release("release migration progress changed repeatedly"),
            Error::Storage,
        ))
    }

    async fn load_versioned(
        &self,
        cell: CellId,
        operation: RequestId,
    ) -> Result<Option<VersionedProgress>> {
        let path = self.path(cell, operation);
        let (bytes, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_PROGRESS_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let progress = MigrationProgress::decode(&bytes, self.identity)?;
        if progress.attempt.cell != cell || progress.attempt.operation != operation {
            return Err(Error::Release(
                "release migration progress is stored at the wrong path",
            ));
        }
        Ok(Some(VersionedProgress { progress, token }))
    }

    fn path(&self, cell: CellId, operation: RequestId) -> object_store::path::Path {
        self.layout
            .migration_path(cell.as_bytes(), operation.as_bytes(), "release.json")
    }
}

fn same_operation(left: &MigrationProgressAttempt, right: &MigrationProgressAttempt) -> bool {
    left.operation == right.operation
        && left.release == right.release
        && left.cell == right.cell
        && left.to_code == right.to_code
        && left.to_schema == right.to_schema
}

fn retryable_storage_error(error: &StorageError) -> bool {
    matches!(
        crab_storage::retry_class(error),
        crab_storage::RetryClass::Transient
            | crab_storage::RetryClass::Throttled { .. }
            | crab_storage::RetryClass::StateDependent
            | crab_storage::RetryClass::InspectErrno
    )
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    decode_hex(value).map_err(|_| Error::Release("invalid migration progress hex field"))
}

fn parse_u64(value: &str) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::Release("invalid migration progress integer"));
    }
    value
        .parse()
        .map_err(|_| Error::Release("invalid migration progress integer"))
}

fn parse_i64(value: &str) -> Result<i64> {
    let parsed = parse_u64(value)?;
    i64::try_from(parsed).map_err(|_| Error::Release("migration progress time overflow"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_storage::Store;
    use object_store::{memory::InMemory, path::Path};

    use super::*;
    use crate::{ApplicationId, TenantId};

    fn fixture() -> (MigrationProgressStore, MigrationProgressAttempt) {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("progress"),
            *identity.application().as_bytes(),
        );
        let attempt = MigrationProgressAttempt::new(
            RequestId::from_bytes([3; 16]),
            Digest::from_bytes([4; 32]),
            SessionId::from_bytes([5; 16]),
            CellId::from_bytes([6; 32]),
            (Digest::from_bytes([7; 32]), 1),
            (Digest::from_bytes([8; 32]), 2),
        )
        .unwrap();
        (
            MigrationProgressStore::new(layout, identity).unwrap(),
            attempt,
        )
    }

    #[tokio::test]
    async fn failure_retry_and_completion_are_canonical_and_monotonic() {
        let (store, attempt) = fixture();
        let failed = store
            .failed(attempt, MigrationFailure::Unavailable, 10)
            .await
            .unwrap();
        assert_eq!(failed.revision(), 1);
        assert_eq!(failed.attempts(), 1);
        assert_eq!(failed.state(), MigrationProgressState::Failed);
        assert_eq!(failed.failure(), Some(MigrationFailure::Unavailable));

        let completed = store.completed(attempt, 20).await.unwrap();
        assert_eq!(completed.revision(), 2);
        assert_eq!(completed.attempts(), 2);
        assert_eq!(completed.state(), MigrationProgressState::Completed);
        assert_eq!(completed.failure(), None);
        assert_eq!(store.completed(attempt, 30).await.unwrap(), completed);
        assert_eq!(
            store
                .failed(attempt, MigrationFailure::Internal, 40)
                .await
                .unwrap(),
            completed
        );
        assert_eq!(
            store
                .load(attempt.cell, attempt.operation)
                .await
                .unwrap()
                .unwrap(),
            completed
        );
    }

    #[tokio::test]
    async fn one_operation_path_rejects_version_scope_drift() {
        let (store, attempt) = fixture();
        store.completed(attempt, 10).await.unwrap();
        let changed = MigrationProgressAttempt::new(
            attempt.operation,
            attempt.release,
            attempt.session,
            attempt.cell,
            (attempt.from_code, attempt.from_schema),
            (Digest::from_bytes([9; 32]), attempt.to_schema),
        )
        .unwrap();
        assert!(store.completed(changed, 20).await.is_err());
    }
}
