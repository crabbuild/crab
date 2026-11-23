use std::collections::BTreeMap;

use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Error, Profile, Result};
use crate::{
    app,
    auth::{Authentication, accounts::Managed},
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
    },
    server::Server,
};

const DIRECTORY_BYTES: usize = 8 * 1024 * 1024;
const DIRECTORY_USERS: usize = 4096;
const ATTEMPTS: usize = 8;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct User {
    pub(super) id: Uuid,
    pub(super) external_id: String,
    pub(super) profile: Profile,
    pub(super) version: u64,
    pub(super) created: String,
    pub(super) updated: String,
    pub(super) deleted: bool,
}

impl User {
    pub(super) fn tag(&self) -> String {
        format!("W/\"{}\"", self.version)
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    users: BTreeMap<Uuid, User>,
    pending: Option<Uuid>,
}

pub(super) struct Directory<'a> {
    pub(super) server: &'a Server,
    pub(super) auth: &'a Authentication,
}

impl<'a> Directory<'a> {
    pub(super) fn new(server: &'a Server) -> Result<Self> {
        let auth = server.auth.as_ref().ok_or_else(Error::not_found)?;
        Ok(Self { server, auth })
    }

    fn path(&self) -> Path {
        let issuer = blake3::hash(self.auth.issuer().as_bytes());
        Path::from(format!(
            "{}/scim/{issuer}/directory",
            self.auth.state.prefix
        ))
    }

    async fn load(&self) -> Result<(Snapshot, Option<ETag>)> {
        match self
            .auth
            .state
            .store
            .get_with_etag_bounded(&self.path(), DIRECTORY_BYTES as u64)
            .await
        {
            Ok((body, etag)) => Ok((
                serde_json::from_slice(&body).map_err(Error::unavailable)?,
                Some(etag),
            )),
            Err(StorageError::NotFound { .. }) => Ok((Snapshot::default(), None)),
            Err(error) => Err(Error::unavailable(error)),
        }
    }

    async fn save(&self, snapshot: &Snapshot, etag: Option<ETag>) -> Result<bool> {
        let body = serde_json::to_vec(snapshot).map_err(Error::unavailable)?;
        if body.len() > DIRECTORY_BYTES || snapshot.users.len() > DIRECTORY_USERS {
            return Err(Error::capacity());
        }
        let body = Bytes::from(body);
        let path = self.path();
        let result = match etag {
            Some(etag) => self
                .auth
                .state
                .store
                .update(&path, body, etag)
                .await
                .map(drop),
            None => self.auth.state.store.create_strict(&path, body).await,
        };
        match result {
            Ok(()) => Ok(true),
            Err(StorageError::StateConflict { .. }) => Ok(false),
            Err(error) => Err(Error::unavailable(error)),
        }
    }

    async fn settled(&self) -> Result<(Snapshot, Option<ETag>)> {
        for _ in 0..ATTEMPTS {
            let (mut snapshot, etag) = self.load().await?;
            let Some(id) = snapshot.pending else {
                return Ok((snapshot, etag));
            };
            let user = snapshot.users.get(&id).ok_or_else(Error::busy)?;
            // The pending entry survives a lost reply and survives restarts. The
            // identity and credential writes are idempotent, and the SCIM CAS
            // prevents an older reconciler from clearing a newer user update.
            if user.version == 1 {
                let identity = IdentityKey {
                    issuer: self.auth.issuer().into(),
                    subject: user.external_id.clone(),
                };
                let cells = self.server.cells().ok_or_else(Error::busy)?;
                cells
                    .command::<RegisterAccounts>(
                        Address::Directory,
                        app::mutation_identity().map_err(Error::unavailable)?,
                        vec![identity],
                    )
                    .await
                    .map_err(Error::unavailable)?;
            }
            self.auth
                .apply_managed(
                    &user.external_id,
                    Managed {
                        id,
                        version: user.version,
                        active: user.profile.active && !user.deleted,
                    },
                )
                .await
                .map_err(Error::unavailable)?;
            snapshot.pending = None;
            self.save(&snapshot, etag).await?;
        }
        Err(Error::busy())
    }

    pub(super) async fn users(&self) -> Result<BTreeMap<Uuid, User>> {
        Ok(self.settled().await?.0.users)
    }

    pub(super) async fn change(
        &self,
        mutation: impl Fn(&mut BTreeMap<Uuid, User>) -> Result<Uuid>,
    ) -> Result<User> {
        for _ in 0..ATTEMPTS {
            let (mut snapshot, etag) = self.settled().await?;
            let id = mutation(&mut snapshot.users)?;
            snapshot.pending = Some(id);
            if self.save(&snapshot, etag).await? {
                self.settled().await?;
                return snapshot.users.remove(&id).ok_or_else(Error::busy);
            }
        }
        Err(Error::busy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn next_read_finishes_committed_deactivation() {
        let harness = crate::test_support::http::Harness::scim().await;
        let directory = Directory {
            server: &harness.server,
            auth: harness.server.auth.as_ref().unwrap(),
        };
        let id = Uuid::now_v7();
        let subject = "unreconciled";
        let profile = Profile::new("retired@example.test".into());
        directory
            .auth
            .apply_managed(
                subject,
                Managed {
                    id,
                    version: 1,
                    active: true,
                },
            )
            .await
            .unwrap();
        let timestamp = super::super::timestamp().unwrap();
        let pending = Snapshot {
            users: BTreeMap::from([(
                id,
                User {
                    id,
                    external_id: subject.into(),
                    profile: Profile {
                        active: false,
                        ..profile
                    },
                    version: 2,
                    created: timestamp.clone(),
                    updated: timestamp,
                    deleted: false,
                },
            )]),
            pending: Some(id),
        };
        assert!(directory.save(&pending, None).await.unwrap());
        assert!(
            !directory
                .auth
                .account_state(subject)
                .await
                .unwrap()
                .suspended
        );
        let restarted = Directory {
            server: &harness.server,
            auth: harness.server.auth.as_ref().unwrap(),
        };
        assert!(!restarted.users().await.unwrap()[&id].profile.active);
        assert!(
            restarted
                .auth
                .account_state(subject)
                .await
                .unwrap()
                .suspended
        );
        assert!(restarted.load().await.unwrap().0.pending.is_none());
        harness.close().await;
    }
}
