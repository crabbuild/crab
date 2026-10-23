use super::{
    AuthError, AuthState, Authentication, Flow, IdentitySessionMarker, Key, STATE_CAS_ATTEMPTS,
    Session, StoredFlow, StoredSession, hex_key, now_epoch,
};
use bytes::Bytes;
use crab_storage::StorageError;
use futures_util::StreamExt;
use object_store::path::Path as ObjectPath;
use openidconnect::{Nonce, PkceCodeVerifier};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl Authentication {
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
        let record = StoredSession {
            identity: session.identity.clone(),
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
        let state = &self.state;
        let body = state
            .store
            .get_with_etag_bounded(&state.path("sessions", &session_key), 64 * 1024)
            .await
            .ok()?
            .0;
        let record: StoredSession = serde_json::from_slice(&body).ok()?;
        let marker_path = state.identity_session_path(
            &record.identity.issuer,
            &record.identity.subject,
            &session_key,
        );
        let (body, _) = state
            .store
            .get_with_etag_bounded(&marker_path, 8 * 1024)
            .await
            .ok()?;
        let marker: IdentitySessionMarker = serde_json::from_slice(&body).ok()?;
        if marker.schema_version != 1
            || marker.session_key != session_key
            || marker.issuer != record.identity.issuer
            || marker.subject != record.identity.subject
            || marker.expires_at != record.expires_at
        {
            return None;
        }
        let session = Arc::new(Session {
            identity: record.identity,
            groups: super::oidc::groups(record.groups).ok()?,
            csrf: record.csrf,
            expires_at: record.expires_at,
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
