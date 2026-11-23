use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AuthError, AuthState, Authentication};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountState {
    pub(crate) revision: u64,
    pub(crate) suspended: bool,
    #[serde(skip)]
    pub(crate) managed: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountRecord {
    revision: u64,
    suspended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed: Option<Managed>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Managed {
    pub(super) id: Uuid,
    pub(super) version: u64,
    pub(super) active: bool,
}

impl Authentication {
    pub(crate) async fn account_state(&self, subject: &str) -> Result<AccountState, AuthError> {
        validate_subject(subject)?;
        self.state.account_state(self.issuer(), subject).await
    }

    pub(crate) async fn set_account_suspended(
        &self,
        subject: &str,
        expected_revision: u64,
        suspended: bool,
    ) -> Result<AccountState, AuthError> {
        validate_subject(subject)?;
        let path = self.state.account_path(self.issuer(), subject);
        let (mut current, etag) = self.state.read_record(&path).await?;
        if current.revision != expected_revision {
            return Err(AuthError::AccountChanged);
        }
        if current.suspended != suspended {
            current.revision = current.revision.checked_add(1).ok_or(AuthError::Invalid)?;
            current.suspended = suspended;
            self.state.write_record(&path, &current, etag).await?;
        }
        Ok(self.state.effective(&current))
    }

    pub(super) async fn apply_managed(
        &self,
        subject: &str,
        managed: Managed,
    ) -> Result<(), AuthError> {
        validate_subject(subject)?;
        let path = self.state.account_path(self.issuer(), subject);
        for _ in 0..8 {
            let (mut current, etag) = self.state.read_record(&path).await?;
            if let Some(previous) = current.managed {
                if previous.id != managed.id {
                    return Err(AuthError::Invalid);
                }
                if previous.version >= managed.version {
                    return Ok(());
                }
            }
            // Preserve the independent operator hold. Advancing the account
            // fence on a lifecycle change denies previously issued credentials.
            if current
                .managed
                .is_none_or(|old| old.active != managed.active)
            {
                current.revision = current.revision.checked_add(1).ok_or(AuthError::Invalid)?;
            }
            current.managed = Some(managed);
            match self.state.write_record(&path, &current, etag).await {
                Err(AuthError::AccountChanged) => continue,
                result => return result,
            }
        }
        Err(AuthError::AccountChanged)
    }
}

impl AuthState {
    pub(super) async fn account_state(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<AccountState, AuthError> {
        self.read_account(&self.account_path(issuer, subject)).await
    }

    pub(super) fn account_path(&self, issuer: &str, subject: &str) -> ObjectPath {
        let mut digest = blake3::Hasher::new_derive_key("crab account state v1");
        digest.update(&(issuer.len() as u64).to_le_bytes());
        digest.update(issuer.as_bytes());
        digest.update(subject.as_bytes());
        ObjectPath::from(format!(
            "{}/{}",
            self.account_prefix,
            digest.finalize().to_hex()
        ))
    }

    pub(super) async fn read_account(&self, path: &ObjectPath) -> Result<AccountState, AuthError> {
        let (record, _) = self.read_record(path).await?;
        Ok(self.effective(&record))
    }

    fn effective(&self, record: &AccountRecord) -> AccountState {
        AccountState {
            revision: record.revision,
            suspended: record.suspended
                || record
                    .managed
                    .map_or(self.require_provisioning, |managed| !managed.active),
            managed: record.managed.is_some(),
        }
    }

    async fn read_record(
        &self,
        path: &ObjectPath,
    ) -> Result<(AccountRecord, Option<ETag>), AuthError> {
        match self.store.get_with_etag_bounded(path, 1024).await {
            Ok((body, etag)) => {
                let state: AccountRecord = serde_json::from_slice(&body)?;
                if state.revision == 0
                    || state
                        .managed
                        .is_some_and(|value| value.id.is_nil() || value.version == 0)
                {
                    return Err(AuthError::Invalid);
                }
                Ok((state, Some(etag)))
            }
            Err(StorageError::NotFound { .. }) => Ok((AccountRecord::default(), None)),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_record(
        &self,
        path: &ObjectPath,
        next: &AccountRecord,
        etag: Option<ETag>,
    ) -> Result<(), AuthError> {
        let body = Bytes::from(serde_json::to_vec(next)?);
        let write = match etag {
            Some(etag) => self.store.update(path, body, etag).await.map(drop),
            None => self.store.create_strict(path, body).await,
        };
        match write {
            Ok(()) => Ok(()),
            Err(StorageError::StateConflict { .. }) => Err(AuthError::AccountChanged),
            Err(error) => Err(error.into()),
        }
    }
}

fn validate_subject(subject: &str) -> Result<(), AuthError> {
    if subject.is_empty() || subject.chars().count() > 1024 || subject.chars().any(char::is_control)
    {
        return Err(AuthError::Invalid);
    }
    Ok(())
}
