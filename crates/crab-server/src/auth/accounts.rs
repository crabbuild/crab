use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};

use super::{AuthError, AuthState, Authentication};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountState {
    pub(crate) revision: u64,
    pub(crate) suspended: bool,
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
        let (current, etag) = self.state.read_account(&path).await?;
        if current.revision != expected_revision {
            return Err(AuthError::AccountChanged);
        }
        if current.suspended == suspended {
            return Ok(current);
        }
        let next = AccountState {
            revision: current.revision.checked_add(1).ok_or(AuthError::Invalid)?,
            suspended,
        };
        let body = Bytes::from(serde_json::to_vec(&next)?);
        let write = match etag {
            Some(etag) => self.state.store.update(&path, body, etag).await.map(drop),
            None => self.state.store.create_strict(&path, body).await,
        };
        match write {
            Ok(()) => Ok(next),
            Err(StorageError::StateConflict { .. }) => Err(AuthError::AccountChanged),
            Err(error) => Err(error.into()),
        }
    }
}

impl AuthState {
    pub(super) async fn account_state(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<AccountState, AuthError> {
        self.read_account(&self.account_path(issuer, subject))
            .await
            .map(|(state, _)| state)
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

    pub(super) async fn read_account(
        &self,
        path: &ObjectPath,
    ) -> Result<(AccountState, Option<ETag>), AuthError> {
        match self.store.get_with_etag_bounded(path, 1024).await {
            Ok((body, etag)) => {
                let state: AccountState = serde_json::from_slice(&body)?;
                if state.revision == 0 {
                    return Err(AuthError::Invalid);
                }
                Ok((state, Some(etag)))
            }
            Err(StorageError::NotFound { .. }) => Ok((AccountState::default(), None)),
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
