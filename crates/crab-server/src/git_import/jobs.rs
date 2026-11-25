use super::{ImportError, ValidatedImport, source};
use crate::{
    catalog::visibility::Visibility, cells::organization::properties::PropertyValue,
    storage_root::StorageRoot,
};
use bytes::Bytes;
use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_ACTIVE_JOBS: usize = 4;
const MAX_JOB_BYTES: u64 = 144 * 1024;
const LEASE: Duration = Duration::from_secs(60);
pub(super) const HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
pub(super) struct Admission {
    active: Arc<Mutex<HashSet<(String, String)>>>,
}

impl Admission {
    pub(super) fn admit(&self, request: &ValidatedImport) -> Result<Slot, ImportError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = (
            request.owner.to_ascii_lowercase(),
            request.name.to_ascii_lowercase(),
        );
        if active.contains(&target) {
            return Err(ImportError::Conflict);
        }
        if active.len() >= MAX_ACTIVE_JOBS {
            return Err(ImportError::Busy);
        }
        active.insert(target.clone());
        Ok(Slot {
            admission: self.clone(),
            target,
        })
    }
}

pub(super) struct Slot {
    admission: Admission,
    target: (String, String),
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.admission
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.target);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum State {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Status {
    pub(super) id: Uuid,
    pub(super) source: String,
    pub(super) owner: String,
    pub(super) name: String,
    #[serde(default)]
    pub(super) visibility: Visibility,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) properties: BTreeMap<Uuid, PropertyValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) fork_source: Option<Uuid>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    default_branch_only: bool,
    state: State,
    message: String,
    #[serde(default)]
    pub(super) resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<Repository>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Repository {
    owner: String,
    name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    pub(super) default_branch: String,
    refs_digest: String,
}

impl Snapshot {
    pub(super) fn new(default_branch: String, refs: &BTreeMap<String, String>) -> Self {
        Self {
            default_branch,
            refs_digest: Self::digest(refs),
        }
    }

    pub(super) fn matches(&self, head: &str, refs: &BTreeMap<String, String>) -> bool {
        head.strip_prefix("refs/heads/") == Some(&self.default_branch)
            && self.refs_digest == Self::digest(refs)
    }

    pub(super) fn publication_digest(&self) -> [u8; 32] {
        let mut hash =
            blake3::Hasher::new_derive_key("crab organization Git import publication v1");
        hash.update(self.default_branch.as_bytes());
        hash.update(&[0]);
        hash.update(self.refs_digest.as_bytes());
        *hash.finalize().as_bytes()
    }

    fn digest(refs: &BTreeMap<String, String>) -> String {
        let mut hash = blake3::Hasher::new_derive_key("crab git import refs v1");
        for (name, oid) in refs {
            // Ref names cannot contain a NUL, so each field has an exact boundary.
            hash.update(name.as_bytes());
            hash.update(&[0]);
            hash.update(oid.as_bytes());
            hash.update(&[0]);
        }
        hash.finalize().to_hex().to_string()
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    digest: String,
    expires_at_ms: u128,
    status: Status,
    #[serde(default)]
    lease: Uuid,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    snapshot: Option<Snapshot>,
}

impl Document {
    fn visible(mut self) -> Result<Status, ImportError> {
        if matches!(self.status.state, State::Queued | State::Running)
            && now_ms()? >= self.expires_at_ms
        {
            self.status.state = State::Interrupted;
            self.status.message = "The import worker stopped reporting progress.".into();
        }
        self.status.resumable = self.description.is_some()
            && matches!(self.status.state, State::Failed | State::Interrupted);
        Ok(self.status)
    }

    fn active(&self) -> Result<bool, ImportError> {
        Ok(matches!(self.status.state, State::Queued | State::Running)
            && now_ms()? < self.expires_at_ms)
    }
}

pub(super) struct Jobs {
    root: StorageRoot,
    scope: Scope,
}

enum Scope {
    Account(blake3::Hash),
    Organization(Uuid),
}

impl Jobs {
    pub(super) fn new(root: &StorageRoot, actor: &crate::auth::Identity) -> Self {
        let mut hash = blake3::Hasher::new_derive_key("crab git import jobs account v1");
        hash.update(actor.issuer.as_bytes());
        hash.update(&[0]);
        hash.update(actor.subject.as_bytes());
        Self {
            root: root.clone(),
            scope: Scope::Account(hash.finalize()),
        }
    }

    pub(super) fn for_organization(root: &StorageRoot, organization: Uuid) -> Self {
        Self {
            root: root.clone(),
            scope: Scope::Organization(organization),
        }
    }

    fn path(&self, id: Uuid) -> object_store::path::Path {
        let scope = match self.scope {
            Scope::Account(actor) => actor.to_hex().to_string(),
            Scope::Organization(organization) => format!("organizations/{organization}"),
        };
        self.root.path(&format!(
            ".crab/http-server/v1/git-imports/{scope}/{id}.json"
        ))
    }

    pub(super) fn repository_id(&self, id: Uuid) -> Uuid {
        let mut hash = blake3::Hasher::new_derive_key("crab git import destination v1");
        match self.scope {
            Scope::Account(actor) => hash.update(actor.as_bytes()),
            Scope::Organization(organization) => hash.update(organization.as_bytes()),
        };
        hash.update(id.as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hash.finalize().as_bytes()[..16]);
        uuid::Builder::from_custom_bytes(bytes).into_uuid()
    }

    async fn document(&self, id: Uuid) -> Result<Option<(Document, ETag)>, ImportError> {
        match self
            .root
            .store
            .get_with_etag_bounded(&self.path(id), MAX_JOB_BYTES)
            .await
        {
            Ok((body, etag)) => Ok(Some((serde_json::from_slice(&body)?, etag))),
            Err(StorageError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(test)]
    pub(super) async fn expire(&self, id: Uuid) {
        let (mut job, etag) = self.document(id).await.unwrap().unwrap();
        job.expires_at_ms = 0;
        self.root
            .store
            .update(
                &self.path(id),
                Bytes::from(serde_json::to_vec(&job).unwrap()),
                etag,
            )
            .await
            .unwrap();
    }

    pub(super) async fn status(&self, id: Uuid) -> Result<Status, ImportError> {
        self.document(id)
            .await?
            .ok_or(ImportError::NotFound)?
            .0
            .visible()
    }

    pub(super) async fn replay(
        &self,
        id: Uuid,
        request: &ValidatedImport,
    ) -> Result<Option<Status>, ImportError> {
        let Some((document, _)) = self.document(id).await? else {
            return Ok(None);
        };
        if document.digest != digest(request)? {
            return Err(ImportError::SubmissionConflict);
        }
        Ok(Some(document.visible()?))
    }

    pub(super) async fn resume_request(
        &self,
        id: Uuid,
        token: Option<String>,
        allowed_hosts: &[String],
    ) -> Result<(Status, Option<ValidatedImport>), ImportError> {
        let (document, _) = self.document(id).await?.ok_or(ImportError::NotFound)?;
        let status = document.clone().visible()?;
        if !status.resumable {
            return Ok((status, None));
        }
        let description = document.description.ok_or(ImportError::Unavailable)?;
        let mut request = if let Some(fork_source) = status.fork_source {
            if token.is_some() {
                return Err(ImportError::Invalid(
                    "A Crab fork uses your signed-in account",
                ));
            }
            ValidatedImport {
                source: status.source.clone(),
                owner: status.owner.clone(),
                name: status.name.clone(),
                description,
                visibility: Visibility::Private,
                properties: BTreeMap::new(),
                fork_source: Some(fork_source),
                default_branch_only: status.default_branch_only,
                token: None,
            }
        } else {
            source::stored_request(
                status.source.clone(),
                status.owner.clone(),
                status.name.clone(),
                description,
                status.visibility,
                token,
                allowed_hosts,
            )?
        };
        request.properties.clone_from(&status.properties);
        if document.digest != digest(&request)? {
            return Err(ImportError::SubmissionConflict);
        }
        Ok((status, Some(request)))
    }

    pub(super) async fn claim(
        &self,
        id: Uuid,
        request: &ValidatedImport,
    ) -> Result<Option<Writer>, ImportError> {
        let (mut document, etag) = self.document(id).await?.ok_or(ImportError::NotFound)?;
        if document.digest != digest(request)? {
            return Err(ImportError::SubmissionConflict);
        }
        if !document.clone().visible()?.resumable {
            return Ok(None);
        }
        document.lease = Uuid::now_v7();
        document.expires_at_ms = now_ms()? + LEASE.as_millis();
        document.status.state = State::Queued;
        document.status.message = "Queued to resume import".into();
        let path = self.path(id);
        match self
            .root
            .store
            .update(&path, Bytes::from(serde_json::to_vec(&document)?), etag)
            .await
        {
            Ok(etag) => Ok(Some(Writer::new(
                self.root.store.clone(),
                path,
                document,
                etag,
            ))),
            Err(StorageError::StateConflict { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn create(
        &self,
        id: Uuid,
        request: &ValidatedImport,
    ) -> Result<Option<Writer>, ImportError> {
        let status = Status {
            id,
            source: request.source.clone(),
            owner: request.owner.clone(),
            name: request.name.clone(),
            visibility: request.visibility,
            properties: request.properties.clone(),
            fork_source: request.fork_source,
            default_branch_only: request.default_branch_only,
            state: State::Queued,
            message: "Queued for import".into(),
            resumable: false,
            repository: None,
        };
        let document = Document {
            digest: digest(request)?,
            expires_at_ms: now_ms()? + LEASE.as_millis(),
            status,
            lease: Uuid::now_v7(),
            description: Some(request.description.clone()),
            snapshot: None,
        };
        let path = self.path(id);
        let store = self.root.store.clone();
        let body = serde_json::to_vec(&document)?;
        // Leave space for the terminal message and projected repository address.
        if body.len() as u64 > MAX_JOB_BYTES - 1024 {
            return Err(ImportError::Invalid("The Git import request is too large"));
        }
        match store
            .create_strict_with_etag(&path, Bytes::from(body))
            .await
        {
            Ok(etag) => Ok(Some(Writer::new(store, path, document, etag))),
            Err(StorageError::StateConflict { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

pub(super) struct Writer {
    store: crab_storage::Store,
    path: object_store::path::Path,
    lease: Uuid,
    document: tokio::sync::Mutex<(Document, ETag)>,
}

impl Writer {
    fn new(
        store: crab_storage::Store,
        path: object_store::path::Path,
        document: Document,
        etag: ETag,
    ) -> Self {
        Self {
            store,
            path,
            lease: document.lease,
            document: tokio::sync::Mutex::new((document, etag)),
        }
    }

    pub(super) async fn queued(&self) -> Status {
        self.document.lock().await.0.status.clone()
    }

    pub(super) async fn snapshot(&self) -> Option<Snapshot> {
        self.document.lock().await.0.snapshot.clone()
    }

    pub(super) async fn pin(&self, snapshot: &Snapshot) -> Result<(), ImportError> {
        let mut stored = self.document.lock().await;
        if let Some(current) = &stored.0.snapshot {
            return if current == snapshot {
                Ok(())
            } else {
                Err(ImportError::SourceChanged)
            };
        }
        let mut document = stored.0.clone();
        document.snapshot = Some(snapshot.clone());
        self.persist(&mut stored, document).await
    }

    pub(super) async fn verify(&self) -> Result<(), ImportError> {
        let (body, _) = self
            .store
            .get_with_etag_bounded(&self.path, MAX_JOB_BYTES)
            .await?;
        let document: Document = serde_json::from_slice(&body)?;
        if document.lease != self.lease || !document.active()? {
            return Err(ImportError::LeaseLost);
        }
        Ok(())
    }

    pub(super) async fn update(&self, state: State, message: &str) -> Result<(), ImportError> {
        let mut stored = self.document.lock().await;
        let mut document = stored.0.clone();
        document.status.state = state;
        document.status.message = message.into();
        if state == State::Succeeded {
            document.status.repository = Some(Repository {
                owner: document.status.owner.clone(),
                name: document.status.name.clone(),
            });
        }
        self.persist(&mut stored, document).await
    }

    pub(super) async fn heartbeat(&self) -> Result<(), ImportError> {
        let mut stored = self.document.lock().await;
        let document = stored.0.clone();
        self.persist(&mut stored, document).await
    }

    async fn persist(
        &self,
        stored: &mut (Document, ETag),
        mut document: Document,
    ) -> Result<(), ImportError> {
        if !stored.0.active()? {
            return Err(ImportError::LeaseLost);
        }
        document.expires_at_ms = now_ms()? + LEASE.as_millis();
        let body = serde_json::to_vec(&document)?;
        // The durable status reader must always be able to read the lease.
        if body.len() as u64 > MAX_JOB_BYTES {
            return Err(ImportError::Invalid(
                "The Git import metadata exceeds the storage limit",
            ));
        }
        // A replaced worker cannot overwrite a newer claim or its result.
        match self
            .store
            .update(&self.path, Bytes::from(body), stored.1.clone())
            .await
        {
            Ok(etag) => {
                *stored = (document, etag);
                Ok(())
            }
            Err(StorageError::StateConflict { .. }) => Err(ImportError::LeaseLost),
            Err(error) => Err(error.into()),
        }
    }
}

fn now_ms() -> Result<u128, ImportError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_millis())
        .map_err(|error| ImportError::Io(std::io::Error::other(error)))
}

fn digest(request: &ValidatedImport) -> Result<String, ImportError> {
    // A credential can change without changing the identity of the operation.
    let intent = (
        &request.source,
        &request.owner,
        &request.name,
        &request.description,
    );
    let encoded = match request.fork_source {
        Some(source) => serde_json::to_vec(&(source, intent))?,
        None => serde_json::to_vec(&intent)?,
    };
    let mut hash = blake3::Hasher::new();
    hash.update(&encoded);
    // Existing private jobs keep their identity. Publication intent is bound
    // separately from replaceable source credentials.
    if request.visibility.is_public() {
        hash.update(b"\0public");
    }
    if request.default_branch_only {
        hash.update(b"\0default-branch-only");
    }
    if !request.properties.is_empty() {
        hash.update(b"\0properties");
        hash.update(&serde_json::to_vec(&request.properties)?);
    }
    Ok(hash.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Identity;
    use crab_storage::Store;
    use serde_json::json;

    #[tokio::test]
    async fn durable_import_jobs_replay_across_instances_and_expire_without_a_worker() {
        let root = StorageRoot::memory(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "import-status",
        );
        let identity = |subject: &str| Identity {
            issuer: "issuer".into(),
            subject: subject.into(),
            name: subject.into(),
        };
        let jobs = Jobs::new(&root, &identity("alice"));
        let second = Jobs::new(&root, &identity("alice"));
        let other = Jobs::new(&root, &identity("bob"));
        let import = ValidatedImport {
            fork_source: None,
            default_branch_only: false,
            source: "https://example.com/repo.git".into(),
            owner: "example".into(),
            name: "repo".into(),
            description: "description".into(),
            visibility: Visibility::Public,
            properties: [(Uuid::now_v7(), PropertyValue::Boolean(false))].into(),
            token: Some("fixture-secret".into()),
        };
        let id = Uuid::now_v7();
        let writer = jobs.create(id, &import).await.unwrap().unwrap();
        assert!(second.create(id, &import).await.unwrap().is_none());
        assert!(matches!(other.status(id).await, Err(ImportError::NotFound)));
        assert_eq!(
            serde_json::to_value(second.status(id).await.unwrap()).unwrap()["state"],
            "queued"
        );
        writer.update(State::Running, "Copying").await.unwrap();
        let (saved, _) = root
            .store
            .get_with_etag_bounded(&jobs.path(id), MAX_JOB_BYTES)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&saved).contains("fixture-secret"));

        second.expire(id).await;
        let interrupted = second.replay(id, &import).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(interrupted).unwrap()["state"],
            "interrupted"
        );
        let mut changed = import.clone();
        changed.visibility = Visibility::Private;
        assert!(matches!(
            second.replay(id, &changed).await,
            Err(ImportError::SubmissionConflict)
        ));
        let (_, resumed) = second
            .resume_request(id, Some("fresh-token".into()), &["example.com".into()])
            .await
            .unwrap();
        let resumed = resumed.unwrap();
        assert_eq!(resumed.visibility, Visibility::Public);
        assert_eq!(resumed.properties, import.properties);
        assert!(second.create(id, &import).await.unwrap().is_none());

        let (left, right) = tokio::join!(jobs.claim(id, &import), second.claim(id, &import));
        let left = left.unwrap();
        let right = right.unwrap();
        assert_ne!(left.is_some(), right.is_some());
        let replacement = left.or(right).unwrap();
        assert!(matches!(writer.verify().await, Err(ImportError::LeaseLost)));
        assert!(matches!(
            writer.heartbeat().await,
            Err(ImportError::LeaseLost)
        ));
        assert!(matches!(
            writer
                .update(State::Succeeded, "Stale worker completed")
                .await,
            Err(ImportError::LeaseLost)
        ));
        replacement
            .update(State::Succeeded, "Import complete")
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(second.status(id).await.unwrap()).unwrap()["repository"],
            json!({"owner":"example","name":"repo"})
        );
    }
}
