use std::time::{SystemTime, UNIX_EPOCH};

use crab_auth::PushRefUpdate;
use crab_git::RepositoryUrl;
use crab_storage::{StorageError, Store, StoreLayout, build_static_env_store};
use object_store::path::Path as ObjectPath;

use super::{
    PreparedViewScope, ProtectedCapsulePushPlan, PushPrepareRecord, build_capsule_prepare_record,
    invalid, receive_provider, validate_prepare_record_shape, validate_push_id,
};
use crate::error::{AuthServerError, Result};

const DEFAULT_MAX_PUSH_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VERIFIED_RECEIVE_BYTES: u64 = 128 * 1024 * 1024;

/// Server-owned context for one protected-push receive session.
pub struct ReceiveContext {
    store: Store,
    router: StoreLayout<Store>,
    repo_prefix: String,
    push_id: String,
    max_push_plan_bytes: u64,
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

    pub(crate) async fn validate_root(&self) -> Result<()> {
        crab_metadata::capsule_protocol::load_root(&self.router)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn read_plan_body(&self) -> Result<bytes::Bytes> {
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
        Ok(body)
    }

    pub(crate) async fn read_plan_document(&self) -> Result<ProtectedCapsulePushPlan> {
        #[derive(serde::Deserialize)]
        struct PlanHeader {
            schema_version: u32,
        }
        let body = self.read_plan_body().await?;
        let header: PlanHeader = serde_json::from_slice(&body)
            .map_err(|e| invalid(format!("invalid push-plan JSON: {e}")))?;
        if header.schema_version
            != crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION
        {
            return Err(invalid("unsupported push-plan schema_version"));
        }
        serde_json::from_slice(&body).map_err(|e| invalid(format!("invalid push-plan JSON: {e}")))
    }

    pub async fn write_prepare_record(
        &self,
        view_ref_updates: Vec<PushRefUpdate>,
        view_scope: Option<PreparedViewScope>,
    ) -> Result<PushPrepareRecord> {
        // Capture only native ref authority before persisting a session; a
        // missing or corrupt root must not authorize legacy metadata or writes.
        let root = crab_metadata::capsule_protocol::load_root(&self.router).await?;
        let view = crab_read::capsule_protocol::open_ref_view_from_root(&self.router, root).await?;
        let record = build_capsule_prepare_record(
            &self.repo_prefix,
            &self.push_id,
            view.root_snapshot().record().root().generation(),
            view.root_snapshot().record().digest(),
            view.refs(),
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
        for path in [self.prepare_record_path(), self.verified_receive_path()] {
            match self.store.delete(&path).await {
                Ok(()) | Err(StorageError::NotFound { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub(crate) async fn write_verified_receive(&self, body: bytes::Bytes) -> Result<()> {
        if body.is_empty() || body.len() as u64 > MAX_VERIFIED_RECEIVE_BYTES {
            return Err(invalid(
                "verified receive evidence exceeds the supported size",
            ));
        }
        self.store
            .put_exact(&self.verified_receive_path(), body)
            .await?;
        Ok(())
    }

    pub(crate) async fn read_verified_receive(&self) -> Result<bytes::Bytes> {
        let path = self.verified_receive_path();
        let meta = self.store.head(&path).await?;
        if meta.size == 0 || meta.size > MAX_VERIFIED_RECEIVE_BYTES {
            return Err(invalid("verified receive evidence has an invalid size"));
        }
        let (body, _) = self.store.get_with_etag(&path).await?;
        Ok(body)
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
        let session_prefix =
            ObjectPath::from(format!("{}/protected-push-sessions/", self.repo_prefix));
        let active_sessions = [self.prepare_record_path(), self.verified_receive_path()];
        for object in self.store.list_prefix(&session_prefix).await? {
            if active_sessions.contains(&object.location)
                || object.last_modified.timestamp() >= cutoff
            {
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

    fn verified_receive_path(&self) -> ObjectPath {
        ObjectPath::from(format!(
            "{}/protected-push-sessions/{}.verified.json",
            self.repo_prefix, self.push_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;
    use crab_metadata::{manifest_store, manifests::Manifest};

    const PUSH_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TEST_MAX_PUSH_PLAN_BYTES: u64 = 1024;

    use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Writes(AtomicU64);

    impl StorageObserver for Writes {
        fn started(&self, operation: StorageOperation) {
            if !matches!(
                operation,
                StorageOperation::Get
                    | StorageOperation::Head
                    | StorageOperation::Range
                    | StorageOperation::List
            ) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn finished(&self, _: StorageObservation) {}
    }

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn context() -> ReceiveContext {
        ReceiveContext::from_store(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            PUSH_ID.to_owned(),
        )
        .with_max_push_plan_bytes(TEST_MAX_PUSH_PLAN_BYTES)
    }

    #[tokio::test]
    async fn legacy_repository_receive_rejects_before_any_write() -> Result<()> {
        let mut outcomes = Vec::new();
        for operation in ["prepare", "verify", "commit"] {
            let store = Store::new(Arc::new(InMemory::new()));
            let layout = StoreLayout::new(store.clone(), "org/repo".to_owned());
            crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout).await?;
            manifest_store::create_manifest(
                &store,
                &layout,
                &Manifest::default_for_repo("refs/heads/main"),
            )
            .await?;
            let writes = Arc::new(Writes::default());
            let observed = store.with_storage_observer(writes.clone());
            let ctx =
                ReceiveContext::from_store(observed, "org/repo".to_owned(), PUSH_ID.to_owned());
            let result = match operation {
                "prepare" => super::super::prepare_receive(
                    &ctx,
                    vec![PushRefUpdate {
                        ref_name: "refs/heads/main".to_owned(),
                        old_oid: None,
                        new_oid: oid('2'),
                    }],
                    None,
                )
                .await
                .map(|_| ()),
                "verify" => super::super::verify_receive(&ctx).await.map(|_| ()),
                _ => super::super::commit_receive(&ctx, "crab://bucket/org/repo", &hash('1'), None)
                    .await
                    .map(|_| ()),
            };
            let refused = matches!(result, Err(AuthServerError::NotFound { ref path }) if path == layout.capsule_root_path().as_ref());
            outcomes.push((refused, writes.0.load(Ordering::Relaxed)));
        }
        assert_eq!(outcomes, [(true, 0); 3]);
        Ok(())
    }

    fn legacy_plan(schema_version: u32) -> crab_remote::protected::ProtectedPushPlan {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.refs.insert("refs/heads/main".to_owned(), oid('2'));
        manifest.seal_git_validation();
        crab_remote::protected::ProtectedPushPlan {
            schema_version,
            mirror_plan_id: (schema_version == 2).then(|| "a".repeat(32)),
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("org/repo/staging/{PUSH_ID}/"),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: oid('2'),
            }],
            candidate_manifest: manifest,
            push_commit_receipt: None,
            staged_objects: Vec::new(),
        }
    }

    #[tokio::test]
    async fn legacy_plan_schemas_are_rejected() -> Result<()> {
        let ctx = context();
        let mut outcomes = Vec::new();
        for schema_version in [1, 2] {
            let plan = legacy_plan(schema_version);
            ctx.store()
                .put_overwrite(
                    &ObjectPath::from(format!("org/repo/staging/{PUSH_ID}/push-plan.json")),
                    Bytes::from(serde_json::to_vec(&plan).unwrap()),
                )
                .await?;
            outcomes.push(ctx.read_plan_document().await.is_err());
        }
        assert_eq!(outcomes, [true, true]);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_plan_admission_precedes_writer_fences() -> Result<()> {
        let mut outcomes = Vec::new();
        for schema in [1, 2] {
            let store = Store::new(Arc::new(InMemory::new()));
            let layout = StoreLayout::new(store.clone(), "org/repo".to_owned());
            crab_write::capsule_protocol::initialize(&layout, &hash('a'), "refs/heads/main")
                .await?;
            let path = layout.repo_path(&format!("staging/{PUSH_ID}/push-plan.json"));
            store
                .put_exact(
                    &path,
                    Bytes::from(serde_json::to_vec(&legacy_plan(schema)).unwrap()),
                )
                .await?;
            let writes = Arc::new(Writes::default());
            let ctx = ReceiveContext::from_store(
                store.with_storage_observer(writes.clone()),
                "org/repo".to_owned(),
                PUSH_ID.to_owned(),
            );
            let result =
                super::super::commit_receive(&ctx, "crab://bucket/org/repo", &hash('1'), None)
                    .await;
            let refused = matches!(result, Err(AuthServerError::Configuration {ref origin, ..}) if origin.contains("unsupported push-plan schema_version"));
            outcomes.push((refused, writes.0.load(Ordering::Relaxed)));
        }
        assert_eq!(outcomes, [(true, 0); 2]);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_prepare_record_cannot_authorize_recovery() -> Result<()> {
        let ctx = context();
        let record = PushPrepareRecord {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            source_manifest_generation: 0,
            source_manifest_etag: "verified-v1-etag".to_owned(),
            source_root_digest: None,
            view_ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: oid('2'),
            }],
            source_ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: oid('2'),
            }],
            view_scope: None,
        };
        ctx.store()
            .put_exact(
                &ctx.prepare_record_path(),
                Bytes::from(serde_json::to_vec(&record).unwrap()),
            )
            .await?;
        assert!(ctx.read_prepare_record().await.is_err());
        Ok(())
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
            .read_plan_document()
            .await
            .expect_err("oversized push plan must be rejected before JSON parsing");

        assert!(
            err.to_string().contains("push-plan.json is too large"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn read_plan_document_selects_capsule_schema() -> Result<()> {
        let ctx = context();
        let path = ObjectPath::from(format!("org/repo/staging/{PUSH_ID}/push-plan.json"));
        let plan = ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("org/repo/staging/{PUSH_ID}/"),
            base_root_digest: hash('1'),
            transaction_id: hash('2'),
            run_hash: hash('3'),
            run_size: 42,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            }],
            staged_objects: Vec::new(),
        };
        ctx.store()
            .put_exact(
                &path,
                Bytes::from(serde_json::to_vec(&plan).expect("serialize capsule plan")),
            )
            .await?;

        let actual = ctx.read_plan_document().await?;

        assert_eq!(actual, plan);
        Ok(())
    }

    #[tokio::test]
    async fn prepare_cleanup_removes_verified_receive_evidence() -> Result<()> {
        let ctx = context();
        ctx.write_verified_receive(Bytes::from_static(b"verified"))
            .await?;

        ctx.cleanup_prepare_record().await?;

        assert!(matches!(
            ctx.read_verified_receive().await,
            Err(AuthServerError::NotFound { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn prepare_record_captures_capsule_root_identity() -> Result<()> {
        let ctx = context();
        let root = crab_metadata::capsule_protocol::RootRecord::encode(
            crab_metadata::capsule_protocol::RepositoryRoot::initial(
                &hash('a'),
                "refs/heads/main",
            )?,
        )?;
        crab_metadata::capsule_protocol::create_root(ctx.router(), root.clone()).await?;
        ctx.validate_root()
            .await
            .expect("v2 receive sessions do not require a legacy layout descriptor");

        let record = ctx
            .write_prepare_record(
                vec![PushRefUpdate {
                    ref_name: "refs/heads/main".to_owned(),
                    old_oid: None,
                    new_oid: oid('2'),
                }],
                None,
            )
            .await?;

        assert_eq!(record.schema_version, 2);
        assert_eq!(record.source_manifest_generation, 0);
        assert!(record.source_manifest_etag.is_empty());
        assert_eq!(record.source_root_digest.as_deref(), Some(root.digest()));
        assert_eq!(record.source_ref_updates[0].old_oid, None);
        validate_prepare_record_shape(&record, "org/repo", PUSH_ID)
    }

    #[tokio::test]
    async fn native_prepare_never_uses_legacy_metadata() -> Result<()> {
        for (valid_native, valid_legacy) in [(true, true), (true, false), (false, true)] {
            let ctx = context();
            let root = crab_metadata::capsule_protocol::RootRecord::encode(
                crab_metadata::capsule_protocol::RepositoryRoot::initial(
                    &hash('a'),
                    "refs/heads/main",
                )?,
            )?;
            crab_metadata::capsule_protocol::create_root(ctx.router(), root.clone()).await?;
            manifest_store::create_manifest(
                ctx.store(),
                ctx.router(),
                &Manifest::default_for_repo("refs/heads/legacy"),
            )
            .await?;
            let corrupt_path = if !valid_native {
                Some(ctx.router().capsule_root_path())
            } else if !valid_legacy {
                Some(ctx.router().manifest_path())
            } else {
                None
            };
            if let Some(path) = corrupt_path {
                ctx.store()
                    .put_overwrite(&path, Bytes::from_static(b"corrupt authority"))
                    .await?;
            }
            let result = ctx
                .write_prepare_record(
                    vec![PushRefUpdate {
                        ref_name: "refs/heads/main".to_owned(),
                        old_oid: None,
                        new_oid: oid('2'),
                    }],
                    None,
                )
                .await;
            let correct = if valid_native {
                result.is_ok_and(|record| {
                    record.schema_version == 2
                        && record.source_root_digest.as_deref() == Some(root.digest())
                })
            } else {
                matches!(result, Err(AuthServerError::CorruptObject { .. }))
                    && matches!(
                        ctx.store().head(&ctx.prepare_record_path()).await,
                        Err(StorageError::NotFound { .. })
                    )
            };
            assert!(correct, "native={valid_native}, legacy={valid_legacy}");
        }
        Ok(())
    }
}
