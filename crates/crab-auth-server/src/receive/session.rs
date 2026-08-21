use std::time::{SystemTime, UNIX_EPOCH};

use crab_auth::PushRefUpdate;
use crab_git::RepositoryUrl;
use crab_metadata::{manifest_store, manifests::Manifest};
use crab_storage::{StorageError, Store, StoreLayout, build_static_env_store};
use object_store::path::Path as ObjectPath;

use super::{
    PreparedViewScope, ProtectedPushPlan, PushPrepareRecord, build_prepare_record, invalid,
    read_verified_staged_object, receive_provider, validate_prepare_record_shape, validate_push_id,
    validate_staged_object_shapes,
};
use crate::error::{AuthServerError, Result};

const DEFAULT_MAX_PUSH_PLAN_BYTES: u64 = 64 * 1024 * 1024;

/// Server-owned context for one protected-push receive session.
pub struct ReceiveContext {
    store: Store,
    router: StoreLayout<Store>,
    repo_prefix: String,
    push_id: String,
    max_push_plan_bytes: u64,
}

/// Source manifest state observed by a receive session.
pub struct BaseState {
    manifest: Manifest,
    etag: String,
}

impl ReceiveContext {
    /// Opens a receive session from helper CLI inputs.
    pub fn open(repo_url: &str, push_id: &str, provider: &str) -> Result<Self> {
        let parsed = RepositoryUrl::parse(repo_url).map_err(AuthServerError::from)?;
        validate_push_id(push_id)?;
        let provider = receive_provider(provider)?;
        let store = build_static_env_store(&parsed.bucket, provider)?;
        Ok(Self::from_store(
            store,
            parsed.repo_prefix,
            push_id.to_owned(),
        ))
    }

    /// Builds a receive session around an already constructed object store.
    pub fn from_store(store: Store, repo_prefix: String, push_id: String) -> Self {
        let router = StoreLayout::new(store.clone(), repo_prefix.clone());
        Self::from_store_and_layout(store, router, repo_prefix, push_id)
    }

    /// Builds a receive session with an explicit immutable-object namespace.
    pub fn from_store_with_global_prefix(
        store: Store,
        repo_prefix: String,
        global_prefix: String,
        push_id: String,
    ) -> Self {
        let router =
            StoreLayout::with_global_prefix(store.clone(), repo_prefix.clone(), global_prefix);
        Self::from_store_and_layout(store, router, repo_prefix, push_id)
    }

    fn from_store_and_layout(
        store: Store,
        router: StoreLayout<Store>,
        repo_prefix: String,
        push_id: String,
    ) -> Self {
        Self {
            store,
            router,
            repo_prefix,
            push_id,
            max_push_plan_bytes: DEFAULT_MAX_PUSH_PLAN_BYTES,
        }
    }

    /// Sets the push-plan size limit for focused tests and constrained callers.
    #[must_use]
    pub fn with_max_push_plan_bytes(mut self, max_push_plan_bytes: u64) -> Self {
        self.max_push_plan_bytes = max_push_plan_bytes;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn router(&self) -> &StoreLayout<Store> {
        &self.router
    }

    pub fn repo_prefix(&self) -> &str {
        &self.repo_prefix
    }

    pub fn push_id(&self) -> &str {
        &self.push_id
    }

    pub async fn read_plan(&self) -> Result<ProtectedPushPlan> {
        let path = ObjectPath::from(format!(
            "{}/staging/{}/push-plan.json",
            self.repo_prefix, self.push_id
        ));
        let meta = self.store.head(&path).await?;
        if meta.size == 0 {
            return Err(invalid("push-plan.json is empty"));
        }
        if meta.size > self.max_push_plan_bytes {
            return Err(invalid("push-plan.json is too large"));
        }
        let (body, _) = self.store.get_with_etag(&path).await?;
        serde_json::from_slice(&body).map_err(|e| invalid(format!("invalid push-plan JSON: {e}")))
    }

    pub fn verified_plan_digest(
        &self,
        plan: &ProtectedPushPlan,
        base: Option<&BaseState>,
    ) -> Result<String> {
        let bytes = serde_json::to_vec_pretty(plan)
            .map_err(|e| AuthServerError::Internal(format!("push-plan serialize: {e}")))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&bytes);
        hasher.update(b"\nsource-generation:");
        match base {
            Some(base) => {
                hasher.update(base.manifest.generation.to_string().as_bytes());
                hasher.update(b"\nsource-etag:");
                hasher.update(base.etag.as_bytes());
            }
            None => {
                hasher.update(b"none");
            }
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    pub async fn read_base_state(&self) -> Result<Option<BaseState>> {
        match read_manifest(&self.store, &self.router).await {
            Ok((manifest, etag)) => Ok(Some(BaseState { manifest, etag })),
            Err(AuthServerError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn write_prepare_record(
        &self,
        view_ref_updates: Vec<PushRefUpdate>,
        view_scope: Option<PreparedViewScope>,
    ) -> Result<PushPrepareRecord> {
        let base = self.read_base_state().await?;
        let record = build_prepare_record(
            &self.repo_prefix,
            &self.push_id,
            base.as_ref().map(|state| (state.manifest(), state.etag())),
            view_ref_updates,
            view_scope,
        )?;
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|e| AuthServerError::Internal(format!("prepare record serialize: {e}")))?;
        self.store
            .put_exact(&self.prepare_record_path(), bytes::Bytes::from(bytes))
            .await?;
        Ok(record)
    }

    pub async fn read_prepare_record(&self) -> Result<PushPrepareRecord> {
        let (body, _) = self
            .store
            .get_with_etag(&self.prepare_record_path())
            .await?;
        let record: PushPrepareRecord = serde_json::from_slice(&body)
            .map_err(|e| invalid(format!("invalid prepare record JSON: {e}")))?;
        validate_prepare_record_shape(&record, &self.repo_prefix, &self.push_id)?;
        Ok(record)
    }

    pub async fn cleanup_prepare_record(&self) -> Result<()> {
        match self.store.delete(&self.prepare_record_path()).await {
            Ok(()) | Err(StorageError::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn validate_staged_objects(&self, plan: &ProtectedPushPlan) -> Result<()> {
        validate_staged_object_shapes(plan, &self.repo_prefix, &self.push_id)?;
        for object in &plan.staged_objects {
            read_verified_staged_object(&self.store, object).await?;
        }
        Ok(())
    }

    pub async fn cleanup_staging(&self) -> Result<()> {
        let prefix = ObjectPath::from(format!("{}/staging/{}/", self.repo_prefix, self.push_id));
        self.store.delete_prefix(&prefix).await?;
        Ok(())
    }

    pub async fn cleanup_expired_staging(&self) -> Result<u64> {
        let ttl_secs = std::env::var("CRAB_AUTH_STAGING_TTL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(24 * 60 * 60);
        if ttl_secs == 0 {
            return Ok(0);
        }
        let ttl_secs = ttl_secs.min(i64::MAX as u64) as i64;

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AuthServerError::Internal(format!("system clock before UNIX epoch: {e}")))?
            .as_secs() as i64;
        let cutoff = now_secs.saturating_sub(ttl_secs);
        let staging_prefix = format!("{}/staging/", self.repo_prefix);
        let active_prefix = format!("{}/staging/{}/", self.repo_prefix, self.push_id);
        let mut deleted = 0;

        for object in self
            .store
            .list_prefix(&ObjectPath::from(staging_prefix))
            .await?
        {
            let key = object.location.as_ref();
            if key.starts_with(&active_prefix) || object.last_modified.timestamp() >= cutoff {
                continue;
            }
            match self.store.delete(&object.location).await {
                Ok(()) => deleted += 1,
                Err(StorageError::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(deleted)
    }

    fn prepare_record_path(&self) -> ObjectPath {
        ObjectPath::from(format!(
            "{}/protected-push-sessions/{}.json",
            self.repo_prefix, self.push_id
        ))
    }
}

impl BaseState {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn etag(&self) -> &str {
        &self.etag
    }
}

async fn read_manifest(store: &Store, router: &StoreLayout<Store>) -> Result<(Manifest, String)> {
    manifest_store::read_manifest(store, router)
        .await
        .map_err(AuthServerError::from)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;
    use crab_storage::StagedWrite;

    const PUSH_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TEST_MAX_PUSH_PLAN_BYTES: u64 = 1024;

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn context() -> ReceiveContext {
        ReceiveContext::from_store(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            PUSH_ID.to_owned(),
        )
        .with_max_push_plan_bytes(TEST_MAX_PUSH_PLAN_BYTES)
    }

    #[test]
    fn explicit_global_prefix_routes_immutable_objects_inside_repository() {
        let context = ReceiveContext::from_store_with_global_prefix(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            "org/repo/.crab".to_owned(),
            PUSH_ID.to_owned(),
        );

        assert_eq!(
            context.router().xorb_path(&hash('a')).as_ref(),
            format!("org/repo/.crab/xorbs/aa/{}", hash('a'))
        );
    }

    fn staged_object(canonical_key: String, bytes: &[u8]) -> StagedWrite {
        StagedWrite {
            staged_key: format!("org/repo/staging/{PUSH_ID}/objects/{canonical_key}"),
            canonical_key,
            blake3: blake3_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    async fn put_staged(ctx: &ReceiveContext, object: &StagedWrite, bytes: &[u8]) -> Result<()> {
        ctx.store()
            .put_exact(
                &ObjectPath::from(object.staged_key.clone()),
                Bytes::copy_from_slice(bytes),
            )
            .await
            .map_err(AuthServerError::from)
    }

    fn plan_with_staged_object(object: StagedWrite) -> ProtectedPushPlan {
        let mut candidate_manifest = Manifest::default_for_repo("refs/heads/main");
        candidate_manifest
            .refs
            .insert("refs/heads/main".to_owned(), oid('2'));

        ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("org/repo/staging/{PUSH_ID}/"),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            }],
            candidate_manifest,
            push_commit_receipt: None,
            staged_objects: vec![object],
        }
    }

    #[test]
    fn open_requires_supported_repo_url_and_safe_prefix() {
        assert!(ReceiveContext::open("crab://bucket/org/repo", PUSH_ID, "s3").is_ok());

        for repo_url in [
            "bucket/org/repo",
            "https://bucket/org/repo",
            "crab://bucket",
            "crab://bucket/org/*",
            "crab://bucket/org/../repo",
        ] {
            assert!(
                ReceiveContext::open(repo_url, PUSH_ID, "s3").is_err(),
                "expected {repo_url} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn read_plan_rejects_oversized_control_file() -> Result<()> {
        let ctx = context();
        let path = ObjectPath::from(format!("org/repo/staging/{PUSH_ID}/push-plan.json"));
        ctx.store()
            .put_exact(
                &path,
                Bytes::from(vec![b' '; TEST_MAX_PUSH_PLAN_BYTES as usize + 1]),
            )
            .await?;

        let err = ctx
            .read_plan()
            .await
            .expect_err("oversized push plan must be rejected before JSON parsing");

        assert!(
            err.to_string().contains("push-plan.json is too large"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn validate_staged_objects_rejects_mismatched_content_addressed_key() -> Result<()> {
        let ctx = context();
        let bytes = b"pack bytes";
        let object = staged_object(format!("org/repo/packs/pack-{}.pack", hash('a')), bytes);
        put_staged(&ctx, &object, bytes).await?;
        let plan = plan_with_staged_object(object);

        let err = ctx
            .validate_staged_objects(&plan)
            .await
            .expect_err("content-addressed staged object key must match staged bytes");

        assert!(
            err.to_string()
                .contains("content-addressed staged object hash mismatch"),
            "unexpected error: {err}"
        );
        Ok(())
    }
}
