use super::{
    AuthError, AuthState, Authentication, Flow, GROUP_SESSION_LIFETIME, IdentitySessionMarker, Key,
    STATE_CAS_ATTEMPTS, Session, StoredFlow, StoredSession, VerifiedSession, hex_key, now_epoch,
    principal::group_access,
};
use crate::RepositoryGroup;
use bytes::Bytes;
use crab_storage::StorageError;
use futures_util::{FutureExt, StreamExt, future::Shared};
use object_store::path::Path as ObjectPath;
use openidconnect::{Nonce, PkceCodeVerifier};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct GroupEvidence(Arc<VerifiedSession>);

impl GroupEvidence {
    pub(crate) fn can_read(&self, grants: &[RepositoryGroup]) -> bool {
        self.0.expires_at > now_epoch().unwrap_or(u64::MAX)
            && group_access(&self.0.groups, grants).is_some()
    }
}

impl Authentication {
    pub(crate) async fn mention_group(
        &self,
        subject: &str,
        grants: &[RepositoryGroup],
    ) -> Result<Option<GroupEvidence>, StorageError> {
        let issuer = self.issuer();
        let state = &self.state;
        let prefix = state.identity_sessions_prefix(issuer, subject);
        let mut markers = state.store.list_stream(&prefix);
        // Old markers can remain after browser expiry. Use metadata to skip
        // stale records, leaving clock skew margin before verifying each session.
        let oldest = now_epoch()
            .unwrap_or(0)
            .saturating_sub(GROUP_SESSION_LIFETIME.as_secs() * 2);
        while let Some(meta) = markers.next().await {
            let meta = meta?;
            if u64::try_from(meta.last_modified.timestamp()).is_ok_and(|time| time < oldest) {
                continue;
            }
            let Some(digest) = meta
                .location
                .filename()
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let Ok(digest) = blake3::Hash::from_hex(digest) else {
                continue;
            };
            let session_key = *digest.as_bytes();
            let paths = state.session_authority_paths(issuer, subject, &session_key);
            if meta.location != paths.0 {
                continue;
            }
            let Some(session) = state.session_check(session_key, Some(paths)).await.await else {
                continue;
            };
            if session.identity.issuer == issuer && session.identity.subject == subject {
                let evidence = GroupEvidence(session);
                if evidence.can_read(grants) {
                    return Ok(Some(evidence));
                }
            }
        }
        Ok(None)
    }

    pub(super) async fn store_flow(&self, state_key: Key, flow: Flow) -> Result<(), AuthError> {
        let state = &self.state;
        let record = StoredFlow {
            provider: flow.provider,
            nonce: flow.nonce.secret().clone(),
            verifier: flow.verifier.secret().clone(),
            return_to: flow.return_to,
            expires_at: flow.expires_at,
            consumed: false,
        };
        state
            .store
            .create_strict(
                &state.path("flows", &state_key),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn take_flow(&self, state_key: Key) -> Result<Flow, AuthError> {
        let state = &self.state;
        let path = state.path("flows", &state_key);
        let (body, etag) = state
            .store
            .get_with_etag_bounded(&path, 64 * 1024)
            .await
            .map_err(|error| match error {
                StorageError::NotFound { .. } => AuthError::Invalid,
                other => AuthError::Storage(other),
            })?;
        let mut record: StoredFlow = serde_json::from_slice(&body)?;
        if record.consumed || record.expires_at <= now_epoch()? {
            let _ = state.store.delete(&path).await;
            return Err(AuthError::Invalid);
        }
        record.consumed = true;
        state
            .store
            .update(&path, Bytes::from(serde_json::to_vec(&record)?), etag)
            .await
            .map_err(|error| match error {
                StorageError::StateConflict { .. } => AuthError::Invalid,
                other => AuthError::Storage(other),
            })?;
        Ok(Flow {
            provider: record.provider,
            nonce: Nonce::new(record.nonce),
            verifier: PkceCodeVerifier::new(record.verifier),
            return_to: record.return_to,
            expires_at: record.expires_at,
        })
    }

    pub(super) async fn store_session(&self, session: Arc<Session>) -> Result<(), AuthError> {
        let state = &self.state;
        let account = state
            .account_state(&session.identity.issuer, &session.identity.subject)
            .await?;
        if account.suspended {
            return Err(AuthError::Suspended);
        }
        let record = StoredSession {
            identity: session.identity.clone(),
            account_revision: account.revision,
            groups: session.groups.clone(),
            csrf: session.csrf.clone(),
            expires_at: session.expires_at,
            git_tokens: Vec::new(),
        };
        let marker_path = state.identity_session_path(
            &session.identity.issuer,
            &session.identity.subject,
            &session.session_key,
        );
        let marker = IdentitySessionMarker {
            schema_version: 1,
            issuer: session.identity.issuer.clone(),
            subject: session.identity.subject.clone(),
            session_key: session.session_key,
            expires_at: session.expires_at,
            git_tokens: Vec::new(),
        };
        state
            .store
            .create_strict(&marker_path, Bytes::from(serde_json::to_vec(&marker)?))
            .await?;
        if let Err(error) = state
            .store
            .create_strict(
                &state.path("sessions", &session.session_key),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await
        {
            let _ = state.store.delete(&marker_path).await;
            return Err(error.into());
        }
        // Revocation fences the marker before removing the parent. Recheck after
        // creation so a login racing that removal cannot publish an unindexed session.
        let verified = async {
            let (body, _) = state
                .store
                .get_with_etag_bounded(&marker_path, 8 * 1024)
                .await?;
            let current: IdentitySessionMarker = serde_json::from_slice(&body)?;
            if current.schema_version != 1
                || current.issuer != marker.issuer
                || current.subject != marker.subject
                || current.session_key != marker.session_key
                || current.expires_at != marker.expires_at
            {
                return Err(AuthError::Invalid);
            }
            // A suspension may have committed while the session was being written.
            if state.account_state(&marker.issuer, &marker.subject).await? != account {
                return Err(AuthError::Suspended);
            }
            Ok(())
        }
        .await;
        if let Err(error) = verified {
            let _ = self.remove_session(session.session_key).await;
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn load_session(&self, session_key: Key) -> Option<Arc<Session>> {
        self.load_session_scoped(session_key, None).await
    }

    pub(super) async fn load_session_scoped(
        &self,
        session_key: Key,
        required_git_token: Option<Key>,
    ) -> Option<Arc<Session>> {
        let state = &self.state;
        let cached_paths = self.sessions.lock().await.get(&session_key).map(|session| {
            let identity = &session.identity;
            state.session_authority_paths(&identity.issuer, &identity.subject, &session_key)
        });
        let read = state.session_check(session_key, cached_paths).await;
        let checked = read.await?;
        // Revocation commits on the parent. Token storage cleanup can finish later.
        if required_git_token.is_some_and(|token| !checked.git_tokens.contains(&token)) {
            return None;
        }
        let session = Arc::new(Session {
            identity: checked.identity.clone(),
            groups: checked.groups.clone(),
            csrf: checked.csrf.clone(),
            expires_at: checked.expires_at,
            session_key,
            revoked: AtomicBool::new(false),
        });
        if session.active() {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, session| session.active());
            if let Some(cached) = sessions.get(&session_key) {
                if cached.identity == session.identity
                    && cached.groups == session.groups
                    && cached.csrf == session.csrf
                    && cached.expires_at == session.expires_at
                {
                    return Some(Arc::clone(cached));
                }
                cached.revoked.store(true, Ordering::Release);
            }
            if sessions.len() < 4096 {
                sessions.insert(session_key, Arc::clone(&session));
            }
            Some(session)
        } else {
            let _ = self.remove_session(session_key).await;
            None
        }
    }

    pub(super) async fn remove_session(&self, session_key: Key) -> Result<(), AuthError> {
        let state = &self.state;
        let path = state.path("sessions", &session_key);
        let mut stored = None;
        for attempt in 0..STATE_CAS_ATTEMPTS {
            let (body, etag) = match state.store.get_with_etag_bounded(&path, 64 * 1024).await {
                Ok(value) => value,
                Err(StorageError::NotFound { .. }) => break,
                Err(error) => return Err(error.into()),
            };
            let mut record: StoredSession = serde_json::from_slice(&body)?;
            // Fence token attachment before taking the cleanup inventory. The
            // durable expired parent rejects new credentials during a retry.
            record.expires_at = 0;
            match state
                .store
                .update(&path, Bytes::from(serde_json::to_vec(&record)?), etag)
                .await
            {
                Ok(_) => {
                    stored = Some(record);
                    break;
                }
                Err(StorageError::StateConflict { .. }) if attempt + 1 < STATE_CAS_ATTEMPTS => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(stored) = stored {
            let marker = IdentitySessionMarker {
                schema_version: 1,
                issuer: stored.identity.issuer,
                subject: stored.identity.subject,
                session_key,
                expires_at: 0,
                git_tokens: stored.git_tokens,
            };
            let marker_path =
                state.identity_session_path(&marker.issuer, &marker.subject, &session_key);
            // Keep the bounded token cleanup inventory after deleting the parent,
            // so a failed deletion can resume through the identity marker.
            state
                .store
                .put_overwrite(&marker_path, Bytes::from(serde_json::to_vec(&marker)?))
                .await?;
            state.delete_if_present(&path).await?;
            state.cleanup_marker(&marker, &marker_path).await?;
        }
        if let Some(session) = self.sessions.lock().await.remove(&session_key) {
            session.revoked.store(true, Ordering::Release);
        }
        self.git_tokens.lock().await.retain(|_, token| {
            if token.session.session_key == session_key {
                token.revoked.store(true, Ordering::Release);
                token.session.revoked.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    pub(crate) async fn revoke_identity(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<u64, AuthError> {
        let state = &self.state;
        let prefix = state.identity_sessions_prefix(issuer, subject);
        let mut stream = state.store.list_stream(&prefix);
        let mut revoked = 0;
        while let Some(meta) = stream.next().await {
            let meta = meta?;
            let (body, etag) = match state
                .store
                .get_with_etag_bounded(&meta.location, 8 * 1024)
                .await
            {
                Ok(value) => value,
                Err(StorageError::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let mut marker: IdentitySessionMarker = serde_json::from_slice(&body)?;
            if marker.schema_version != 1
                || marker.issuer != issuer
                || marker.subject != subject
                || meta.location
                    != state.identity_session_path(issuer, subject, &marker.session_key)
            {
                return Err(AuthError::Invalid);
            }
            // Fence in-progress creation before inspecting the parent: otherwise
            // removing a stale-looking marker can race the session's first write.
            marker.expires_at = 0;
            state
                .store
                .update(
                    &meta.location,
                    Bytes::from(serde_json::to_vec(&marker)?),
                    etag,
                )
                .await?;
            let parent_path = state.path("sessions", &marker.session_key);
            match state
                .store
                .get_with_etag_bounded(&parent_path, 64 * 1024)
                .await
            {
                Ok((body, _)) => {
                    let parent: StoredSession = serde_json::from_slice(&body)?;
                    if parent.identity.issuer != issuer || parent.identity.subject != subject {
                        return Err(AuthError::Invalid);
                    }
                }
                Err(StorageError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            self.remove_session(marker.session_key).await?;
            state.cleanup_marker(&marker, &meta.location).await?;
            revoked += 1;
        }
        Ok(revoked)
    }
}

impl AuthState {
    async fn session_check(
        &self,
        session_key: Key,
        cached_paths: Option<(ObjectPath, ObjectPath)>,
    ) -> Shared<super::SessionCheck> {
        let mut pending = self.pending_checks.lock().await;
        if let Some(read) = pending.get(&session_key).and_then(|read| read.upgrade()) {
            return read;
        }
        if pending.len() >= 4096 {
            pending.retain(|_, read| read.upgrade().is_some());
        }
        // At capacity the same fresh check runs without retaining another key.
        let registered = pending.len() < 4096;
        let state = self.clone();
        let read = async move {
            tokio::task::yield_now().await;
            // Enrollment ends before either provider read starts. Later arrivals
            // must observe fresh durable state, including completed revocations.
            if registered {
                state.pending_checks.lock().await.remove(&session_key);
            }
            state.verify_session(session_key, cached_paths).await
        }
        .boxed()
        .shared();
        // Weak entries cannot retain a producer after its last caller leaves.
        if registered && let Some(weak) = read.downgrade() {
            pending.insert(session_key, weak);
        }
        read
    }

    async fn verify_session(
        &self,
        session_key: Key,
        cached_paths: Option<(ObjectPath, ObjectPath)>,
    ) -> Option<Arc<VerifiedSession>> {
        let session_path = self.path("sessions", &session_key);
        let record = self.store.get_with_etag_bounded(&session_path, 64 * 1024);
        let cached = async {
            let paths = cached_paths.as_ref()?;
            self.read_session_authority(paths).await
        };
        // Cached addresses only overlap I/O. The current record selects the
        // session marker and account revision before any grant is admitted.
        let (record, cached) = tokio::join!(record, cached);
        let record: StoredSession = serde_json::from_slice(&record.ok()?.0).ok()?;
        let identity = &record.identity;
        let paths = self.session_authority_paths(&identity.issuer, &identity.subject, &session_key);
        let (marker, account) = if cached_paths.as_ref() == Some(&paths) {
            cached?
        } else {
            self.read_session_authority(&paths).await?
        };
        if marker.schema_version != 1
            || marker.session_key != session_key
            || marker.issuer != record.identity.issuer
            || marker.subject != record.identity.subject
            || marker.expires_at != record.expires_at
            || account.suspended
            || account.revision != record.account_revision
        {
            return None;
        }
        Some(Arc::new(VerifiedSession {
            identity: record.identity,
            groups: super::oidc::groups(record.groups).ok()?,
            csrf: record.csrf,
            expires_at: record.expires_at,
            git_tokens: record.git_tokens,
        }))
    }

    fn session_authority_paths(
        &self,
        issuer: &str,
        subject: &str,
        session_key: &Key,
    ) -> (ObjectPath, ObjectPath) {
        (
            self.identity_session_path(issuer, subject, session_key),
            self.account_path(issuer, subject),
        )
    }

    async fn read_session_authority(
        &self,
        (marker_path, account_path): &(ObjectPath, ObjectPath),
    ) -> Option<(IdentitySessionMarker, super::AccountState)> {
        let (marker, account) = tokio::join!(
            self.store.get_with_etag_bounded(marker_path, 8 * 1024),
            self.read_account(account_path),
        );
        Some((serde_json::from_slice(&marker.ok()?.0).ok()?, account.ok()?))
    }

    pub(super) async fn cleanup_marker(
        &self,
        marker: &IdentitySessionMarker,
        path: &ObjectPath,
    ) -> Result<(), AuthError> {
        for key in &marker.git_tokens {
            self.delete_if_present(&self.path("git-tokens", key))
                .await?;
        }
        self.delete_if_present(path).await
    }
    pub(super) async fn delete_if_present(&self, path: &ObjectPath) -> Result<(), AuthError> {
        match self.store.delete(path).await {
            Ok(()) | Err(StorageError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn path(&self, kind: &str, key: &Key) -> ObjectPath {
        ObjectPath::from(format!("{}/{kind}/{}", self.prefix, hex_key(key)))
    }

    pub(super) fn identity_sessions_prefix(&self, issuer: &str, subject: &str) -> ObjectPath {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab identity sessions v1\0");
        hasher.update(issuer.as_bytes());
        hasher.update(b"\0");
        hasher.update(subject.as_bytes());
        ObjectPath::from(format!(
            "{}/identity-sessions/{}/",
            self.prefix,
            hasher.finalize().to_hex()
        ))
    }

    pub(super) fn identity_session_path(
        &self,
        issuer: &str,
        subject: &str,
        key: &Key,
    ) -> ObjectPath {
        ObjectPath::from(format!(
            "{}/{}.json",
            self.identity_sessions_prefix(issuer, subject),
            hex_key(key)
        ))
    }
}
