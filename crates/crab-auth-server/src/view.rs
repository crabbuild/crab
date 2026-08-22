use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_auth::managed::SecretString;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_git::{CrabUrl, lfs_pointer::LfsPointer, verify_pack_sha1};
use crab_lfs::LfsObjectStore;
use crab_metadata::{
    file_index_lookup::resolve_file_hash_to_shard,
    manifest_store,
    manifests::{BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index},
    pack_metadata::PackMetadata,
};
use crab_read::ShardHydrator;
use crab_storage::{StorageProviderKind, Store, build_static_env_store};
use crab_types::pointer::Pointer;
use crab_types::time::now_rfc3339_millis;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::ShardReader;
use serde::Serialize;

use crate::error::{AuthServerError, Result};

mod git_workspace;
mod objects;
mod repack;

use git_workspace::{
    ViewGitWorkspace, clone_bare, count_pack_objects, generate_view_pack, list_view_refs,
    resolve_view_head, scan_reachable_pointers,
};
use objects::{commit_view_metadb, upload_view_crab_objects};
use repack::{ViewCrabObjects, ViewCrabRepacker, materialize_crab_pointers_in_fast_export};

#[cfg(test)]
use git_workspace::{path_str, run_git, run_git_capture, run_git_owned};

type StoreLayout = crab_storage::StoreLayout<Store>;

async fn read_manifest(store: &Store, router: &StoreLayout) -> Result<(Manifest, String)> {
    manifest_store::read_manifest(store, router)
        .await
        .map_err(AuthServerError::from)
}

async fn read_repository_snapshot(
    store: &Store,
    router: &StoreLayout,
) -> Result<manifest_store::RepositorySnapshot> {
    manifest_store::read_repository_snapshot(store, router)
        .await
        .map_err(AuthServerError::from)
}

#[cfg(test)]
async fn read_bulk_pack_list(
    store: &Store,
    router: &StoreLayout,
    hash: &str,
) -> Result<Vec<PackManifestEntry>> {
    manifest_store::read_bulk_pack_list(store, router, hash)
        .await
        .map_err(AuthServerError::from)
}

async fn upload_segmented_bulk(store: &Store, router: &StoreLayout, bulk: &BulkData) -> Result<()> {
    manifest_store::upload_segmented_bulk(store, router, bulk)
        .await
        .map_err(AuthServerError::from)
}

async fn create_manifest(store: &Store, router: &StoreLayout, manifest: &Manifest) -> Result<()> {
    manifest_store::create_manifest(store, router, manifest)
        .await
        .map_err(AuthServerError::from)
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewOutput {
    repo_prefix: String,
    global_prefix: String,
    source_repo: String,
    scope_hash: String,
    source_generation: u64,
    source_manifest_hash: String,
    cache_hit: bool,
}

#[derive(Clone)]
pub struct ViewS3Credentials {
    access_key_id: SecretString,
    secret_access_key: SecretString,
    session_token: Option<SecretString>,
    region: String,
}

impl ViewS3Credentials {
    /// Creates credentials used only by the view builder's Git child process.
    #[must_use]
    pub fn new(
        access_key_id: SecretString,
        secret_access_key: SecretString,
        session_token: Option<SecretString>,
        region: String,
    ) -> Self {
        Self {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        }
    }

    fn apply(&self, command: &mut std::process::Command) {
        command
            .env("AWS_ACCESS_KEY_ID", self.access_key_id.expose_secret())
            .env(
                "AWS_SECRET_ACCESS_KEY",
                self.secret_access_key.expose_secret(),
            )
            .env("AWS_REGION", &self.region)
            .env("AWS_DEFAULT_REGION", &self.region);
        if let Some(session_token) = self.session_token.as_ref() {
            command.env("AWS_SESSION_TOKEN", session_token.expose_secret());
        } else {
            command.env_remove("AWS_SESSION_TOKEN");
        }
    }
}

impl std::fmt::Debug for ViewS3Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ViewS3Credentials(<redacted>)")
    }
}

impl ViewOutput {
    /// Returns the object-storage prefix containing the filtered repository.
    #[must_use]
    pub fn repo_prefix(&self) -> &str {
        &self.repo_prefix
    }

    /// Returns the filtered repository global metadata prefix.
    #[must_use]
    pub fn global_prefix(&self) -> &str {
        &self.global_prefix
    }

    /// Returns the source repository prefix represented by this view.
    #[must_use]
    pub fn source_repo(&self) -> &str {
        &self.source_repo
    }

    /// Returns the canonical policy scope hash for this view.
    #[must_use]
    pub fn scope_hash(&self) -> &str {
        &self.scope_hash
    }
}

/// Materializes or verifies a path-scoped protected view.
pub async fn materialize_view(
    repo_url: &str,
    provider: &str,
    scope_hash: &str,
    read_paths: &[String],
    deny_paths: &[String],
) -> Result<ViewOutput> {
    let parsed = CrabUrl::parse(repo_url).map_err(AuthServerError::from)?;
    let store = build_view_store(&parsed.bucket, provider)?;
    materialize_view_with_store(repo_url, scope_hash, read_paths, deny_paths, store).await
}

/// Materializes a path-scoped protected view using a caller-owned object store.
pub async fn materialize_view_with_store(
    repo_url: &str,
    scope_hash: &str,
    read_paths: &[String],
    deny_paths: &[String],
    store: Store,
) -> Result<ViewOutput> {
    materialize_view_with_store_and_credentials(
        repo_url, scope_hash, read_paths, deny_paths, store, None,
    )
    .await
}

/// Materializes a protected view with credentials confined to Git child processes.
pub async fn materialize_view_with_store_and_credentials(
    repo_url: &str,
    scope_hash: &str,
    read_paths: &[String],
    deny_paths: &[String],
    store: Store,
    git_credentials: Option<ViewS3Credentials>,
) -> Result<ViewOutput> {
    validate_scope_hash(scope_hash)?;
    let include = normalize_pathspecs(read_paths, "read_path")?;
    let deny = normalize_pathspecs(deny_paths, "deny_path")?;
    if include.is_empty() {
        return Err(AuthServerError::AuthFailed {
            path: "path-scoped read view requires at least one read path".into(),
        });
    }

    let parsed = CrabUrl::parse(repo_url).map_err(AuthServerError::from)?;
    let source_router = StoreLayout::new(store.clone(), parsed.repo_path.clone());
    let snapshot = read_repository_snapshot(&store, &source_router).await?;
    let manifest = snapshot.manifest;
    let source_manifest_hash = snapshot.journal.state_digest;
    let repo_prefix = view_prefix(
        &parsed.repo_path,
        scope_hash,
        manifest.generation,
        &source_manifest_hash,
    );
    let global_prefix = format!("{repo_prefix}/.crab");
    let output = ViewOutput {
        repo_prefix: repo_prefix.clone(),
        global_prefix,
        source_repo: parsed.repo_path.clone(),
        scope_hash: scope_hash.to_ascii_lowercase(),
        source_generation: manifest.generation,
        source_manifest_hash,
        cache_hit: false,
    };

    let view_router = StoreLayout::new(store.clone(), repo_prefix.clone());
    if read_manifest(&store, &view_router).await.is_ok() {
        verify_existing_view(
            &parsed.bucket,
            &repo_prefix,
            &store,
            &parsed.repo_path,
            git_credentials.as_ref(),
        )
        .await?;
        return Ok(ViewOutput {
            cache_hit: true,
            ..output
        });
    }

    build_filtered_view(
        repo_url,
        &parsed.repo_path,
        &repo_prefix,
        manifest.generation,
        &store,
        &include,
        &deny,
        git_credentials.as_ref(),
    )
    .await?;
    read_manifest(&store, &view_router)
        .await
        .map_err(|e| AuthServerError::AuthFailed {
            path: format!("filtered view push did not produce a manifest: {e}"),
        })?;

    Ok(output)
}

fn build_view_store(bucket: &str, provider: &str) -> Result<Store> {
    let provider = StorageProviderKind::parse_cloud_alias(provider).ok_or_else(|| {
        AuthServerError::AuthFailed {
            path: format!("unsupported view provider: {}", provider.trim()),
        }
    })?;
    Ok(build_static_env_store(bucket, provider)?)
}

async fn build_filtered_view(
    source_url: &str,
    source_repo: &str,
    repo_prefix: &str,
    view_generation: u64,
    store: &Store,
    include: &[String],
    deny: &[String],
    git_credentials: Option<&ViewS3Credentials>,
) -> Result<()> {
    let workspace = ViewGitWorkspace::create(source_url, include, deny, git_credentials)?;
    let hydrator = source_hydrator(store.clone(), source_repo, workspace.temp_path())?;
    let mut repacker = ViewCrabRepacker::new(hydrator);
    materialize_crab_pointers_in_fast_export(
        workspace.export_stream(),
        workspace.repacked_stream(),
        &mut repacker,
    )
    .await?;
    workspace.import_repacked_history()?;
    workspace.validate_git_state()?;
    publish_filtered_view(
        source_repo,
        repo_prefix,
        view_generation,
        store,
        workspace.filtered_git(),
        repacker.finish()?,
    )
    .await?;
    verify_filtered_view_content(workspace.filtered_git(), store, source_repo, repo_prefix).await
}

fn source_hydrator(store: Store, source_repo: &str, temp_dir: &Path) -> Result<ShardHydrator> {
    let cache = Arc::new(crab_cache::LocalCache::new(temp_dir.join("hydrate-cache")));
    let caching = CachingStore::new_with_local_cache(store.clone(), CacheConfig::default(), cache)?;
    let router = StoreLayout::new(store, source_repo.to_owned());
    ShardHydrator::new(caching, router, 16).map_err(read_error)
}

fn read_error(error: crab_read::ReadError) -> AuthServerError {
    match error {
        crab_read::ReadError::Io(source) => AuthServerError::Io(source),
        crab_read::ReadError::NotFound { path } => AuthServerError::NotFound { path },
        crab_read::ReadError::HashMismatch { requested, actual } => {
            AuthServerError::HashMismatch { requested, actual }
        }
        other => AuthServerError::Internal(other.to_string()),
    }
}

async fn verify_existing_view(
    bucket: &str,
    repo_prefix: &str,
    store: &Store,
    source_repo: &str,
    git_credentials: Option<&ViewS3Credentials>,
) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let view_git = temp.path().join("view.git");
    let view_url = format!("crab://{bucket}/{repo_prefix}");
    clone_bare(&view_url, &view_git, git_credentials)?;
    verify_filtered_view_content(&view_git, store, source_repo, repo_prefix).await
}

async fn verify_filtered_view_content(
    filtered_git: &Path,
    store: &Store,
    source_repo: &str,
    repo_prefix: &str,
) -> Result<()> {
    let scan = scan_reachable_pointers(filtered_git)?;
    verify_crab_pointers_backed_by_view(store, repo_prefix, &scan.crab_pointers).await?;
    copy_lfs_objects(store.clone(), source_repo, repo_prefix, &scan.lfs_pointers).await
}

async fn verify_crab_pointers_backed_by_view(
    store: &Store,
    repo_prefix: &str,
    pointers: &[Pointer],
) -> Result<()> {
    if pointers.is_empty() {
        return Ok(());
    }

    let router = view_store_layout(store, repo_prefix);
    let mut seen = HashSet::new();
    for pointer in pointers {
        let file_hash = MerkleHash::from(pointer.file_hash);
        if !seen.insert(file_hash) {
            continue;
        }
        let shard_hash =
            resolve_file_hash_to_shard(Arc::clone(store.inner()), router.repo_prefix(), &file_hash)
                .await?
                .ok_or_else(|| AuthServerError::AuthFailed {
                    path: format!(
                        "filtered ACL view pointer {} has no view-local file-index entry",
                        file_hash.hex()
                    ),
                })?;
        let (shard_bytes, _) = store.get_with_etag(&router.shard_path(&shard_hash)).await?;
        let shard = ShardReader::from_bytes(shard_bytes, shard_hash);
        let file_info =
            shard
                .get_file_info(&file_hash)?
                .ok_or_else(|| AuthServerError::AuthFailed {
                    path: format!(
                        "filtered ACL view shard {} does not cover pointer {}",
                        shard_hash.hex(),
                        file_hash.hex()
                    ),
                })?;
        for segment in &file_info.segments {
            store.head(&router.xorb_path(&segment.xorb_hash)).await?;
        }
    }
    Ok(())
}

async fn verify_crab_pointers_backed_by_uploaded_view(
    store: &Store,
    router: &StoreLayout,
    shard_hashes: &[String],
    pointers: &[Pointer],
) -> Result<()> {
    for pointer in pointers {
        let file_hash = MerkleHash::from(pointer.file_hash);
        let mut matched = None;
        for shard_hash in shard_hashes {
            let shard_hash = MerkleHash::from_hex(shard_hash).map_err(|error| {
                AuthServerError::Internal(format!("invalid uploaded view shard hash: {error}"))
            })?;
            let path = router.shard_path(&shard_hash);
            let (bytes, _) = store.get_with_etag(&path).await?;
            if crab_xet::hash::compute_data_hash(&bytes) != shard_hash {
                return Err(AuthServerError::CorruptObject {
                    path: path.to_string(),
                    reason: "uploaded ACL view shard hash mismatch".to_owned(),
                });
            }
            let shard = ShardReader::from_bytes(bytes, shard_hash);
            if let Some(file_info) = shard.get_file_info(&file_hash)? {
                matched = Some(file_info);
                break;
            }
        }
        let file_info = matched.ok_or_else(|| AuthServerError::AuthFailed {
            path: format!(
                "filtered ACL view pointer {} has no uploaded shard recipe",
                file_hash.hex()
            ),
        })?;
        for segment in &file_info.segments {
            store.head(&router.xorb_path(&segment.xorb_hash)).await?;
        }
    }
    Ok(())
}

async fn publish_filtered_view(
    source_repo: &str,
    repo_prefix: &str,
    view_generation: u64,
    store: &Store,
    filtered_git: &Path,
    crab_objects: ViewCrabObjects,
) -> Result<()> {
    let router = view_store_layout(store, repo_prefix);
    let uploaded_crab = upload_view_crab_objects(store, &router, crab_objects).await?;
    let refs = list_view_refs(filtered_git)?;
    let head = resolve_view_head(filtered_git, &refs)?;
    let ref_pairs = refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let peeled_refs = crate::receive::derive_peeled_refs(filtered_git, &ref_pairs)?;
    let scan = scan_reachable_pointers(filtered_git)?;
    copy_lfs_objects(store.clone(), source_repo, repo_prefix, &scan.lfs_pointers).await?;
    verify_crab_pointers_backed_by_uploaded_view(
        store,
        &router,
        &uploaded_crab.shard_hashes,
        &scan.crab_pointers,
    )
    .await?;
    let pack = upload_view_git_pack(store, &router, filtered_git, &refs).await?;
    let packs_for_index: Vec<PackManifestEntry> = pack.iter().cloned().collect();
    let manifest = prepare_view_manifest(
        store,
        &router,
        view_generation,
        refs,
        head,
        peeled_refs,
        uploaded_crab.shard_hashes.clone(),
        pack,
    )
    .await?;
    let visibility_publication = crate::receive::publish_git_visibility_index_from_git_dir(
        store,
        &router,
        &manifest,
        filtered_git,
    )
    .await?;
    if let crate::receive::GitVisibilityPublication::CompletePackOnly { observed, maximum } =
        visibility_publication
    {
        tracing::warn!(
            generation = manifest.generation,
            proof_objects = observed,
            maximum,
            "ACL view exceeds the Git visibility proof profile; complete-pack fetch remains available"
        );
    }
    let gc_registry_generation = crab_metadata::ref_registry::union_register_repo_shards(
        store,
        &router,
        uploaded_crab.shard_hashes.clone(),
    )
    .await?;
    create_manifest(store, &router, &manifest).await?;
    let file_index_digest = match commit_view_metadb(
        store,
        &router,
        &uploaded_crab,
        &manifest,
        gc_registry_generation,
    )
    .await
    {
        Ok(digest) => Some(digest),
        Err(error) => {
            tracing::warn!(
                error = %error,
                generation = manifest.generation,
                "ACL view committed; metadata acceleration requires repair"
            );
            None
        }
    };
    let git_object_locator_digest = match crate::receive::commit_service_git_locators(
        store,
        &router,
        &manifest,
        &packs_for_index,
    )
    .await
    {
        Ok(digest) => Some(digest),
        Err(error) => {
            tracing::warn!(
                error = %error,
                generation = manifest.generation,
                "ACL view committed; Git locator acceleration requires repair"
            );
            None
        }
    };
    if let (Some(file_index_digest), Some(git_object_locator_digest)) =
        (file_index_digest, git_object_locator_digest)
        && let Err(error) = crate::receive::write_service_generation_index_receipt(
            store,
            &router,
            &manifest,
            file_index_digest,
            git_object_locator_digest,
        )
        .await
    {
        tracing::warn!(
            error = %error,
            generation = manifest.generation,
            "ACL view committed; generation receipt requires repair"
        );
    }
    Ok(())
}

async fn upload_view_git_pack(
    store: &Store,
    router: &StoreLayout,
    filtered_git: &Path,
    refs: &BTreeMap<String, String>,
) -> Result<Option<PackManifestEntry>> {
    if refs.is_empty() {
        return Ok(None);
    }

    let pack_bytes = generate_view_pack(filtered_git)?;
    verify_pack_sha1(&pack_bytes).map_err(AuthServerError::from)?;
    let object_count = count_pack_objects(&pack_bytes);
    if object_count == 0 {
        return Ok(None);
    }

    let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
    let pack_path = router.pack_path(&pack_id);
    let pack_size = pack_bytes.len() as u64;
    store.put(&pack_path, Bytes::from(pack_bytes)).await?;

    let ref_tips: Vec<String> = refs.values().cloned().collect();
    let metadata = PackMetadata {
        pack_id: pack_id.clone(),
        ref_tips: ref_tips.clone(),
        object_count,
    };
    let meta_json = serde_json::to_vec(&metadata)
        .map_err(|e| AuthServerError::Internal(format!("pack metadata serialize: {e}")))?;
    let meta_path = router.pack_metadata_path(&pack_id);
    store.put(&meta_path, Bytes::from(meta_json)).await?;

    let entry = PackManifestEntry {
        pack_id: pack_id.clone(),
        size: pack_size,
        content_hash: pack_id,
        ref_tips,
        object_count,
    };
    crab_metadata::pack_origin::record_verified_pack_origin(store, router.repo_prefix(), &entry)
        .await?;
    Ok(Some(entry))
}

fn view_store_layout(store: &Store, repo_prefix: &str) -> StoreLayout {
    StoreLayout::with_global_prefix(
        store.clone(),
        repo_prefix.to_owned(),
        format!("{}/.crab", repo_prefix.trim_end_matches('/')),
    )
}

async fn prepare_view_manifest(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
    refs: BTreeMap<String, String>,
    head: String,
    peeled_refs: BTreeMap<String, String>,
    shard_hashes: Vec<String>,
    pack: Option<PackManifestEntry>,
) -> Result<Manifest> {
    let packs: Vec<PackManifestEntry> = pack.into_iter().collect();
    let (shard_index_hash, _, shard_index) = compact_shard_index(generation, &shard_hashes)?;
    let (pack_index_hash, _, pack_index) = compact_pack_index(generation, &packs)?;
    let bulk = BulkData {
        shard_index,
        pack_index,
    };
    upload_segmented_bulk(store, router, &bulk).await?;

    let mut manifest = Manifest {
        version: 2,
        generation,
        created_at: now_rfc3339_millis(),
        pusher: Some("crab-auth-view".to_owned()),
        session_id: uuid::Uuid::now_v7().to_string(),
        refs,
        peeled_refs,
        head,
        shard_index_hash,
        pack_index_hash,
        git_validation_digest: String::new(),
        commit_graph_hash: None,
        ref_registry_hash: None,
    };
    // View packs are generated from the already validated source workspace;
    // bind the final filtered refs and compacted inventory atomically.
    manifest.seal_git_validation();
    Ok(manifest)
}

async fn copy_lfs_objects(
    store: Store,
    source_repo: &str,
    repo_prefix: &str,
    pointers: &[LfsPointer],
) -> Result<()> {
    if pointers.is_empty() {
        return Ok(());
    }

    let source_lfs = LfsObjectStore::new(store.clone(), source_repo);
    let view_lfs = LfsObjectStore::new(store, repo_prefix);
    for pointer in pointers {
        let bytes = source_lfs.verify(&pointer.oid).await?;
        view_lfs.put(&pointer.oid, bytes).await?;
    }
    Ok(())
}

fn normalize_pathspecs(paths: &[String], field: &str) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        normalized.push(normalize_pathspec(path, field)?);
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_pathspec(path: &str, field: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty()
        || trimmed != path
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.contains("//")
        || trimmed.chars().any(char::is_control)
        || trimmed
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(AuthServerError::AuthFailed {
            path: format!("invalid ACL {field} pathspec"),
        });
    }
    Ok(trimmed.to_owned())
}

fn validate_scope_hash(scope_hash: &str) -> Result<()> {
    if scope_hash.len() == 64 && scope_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(AuthServerError::AuthFailed {
            path: "invalid ACL view scope_hash".into(),
        })
    }
}

fn view_prefix(
    source_repo: &str,
    scope_hash: &str,
    generation: u64,
    manifest_hash: &str,
) -> String {
    format!(
        "{}/acl-views/v1/{}/{}-{}",
        source_repo.trim_matches('/'),
        scope_hash.to_ascii_lowercase(),
        generation,
        manifest_hash
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::fs::File;
    use std::process::Stdio;
    use std::sync::Arc;

    use bytes::Bytes;
    use crab_xet::chunker::GearChunker;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use sha2::Digest;

    use super::repack::RepackedFile;
    use super::*;

    #[test]
    fn view_prefix_includes_source_scope_generation_and_manifest_hash() {
        let prefix = view_prefix("org/repo", &"A".repeat(64), 7, "deadbeef");

        assert_eq!(
            prefix,
            format!("org/repo/acl-views/v1/{}/7-deadbeef", "a".repeat(64))
        );
    }

    #[test]
    fn normalize_pathspecs_deduplicates_and_sorts() {
        let paths = normalize_pathspecs(
            &[
                "src/**".to_owned(),
                "README.md".to_owned(),
                "src/**".to_owned(),
            ],
            "read_path",
        )
        .unwrap();

        assert_eq!(paths, vec!["README.md".to_owned(), "src/**".to_owned()]);
    }

    #[test]
    fn normalize_pathspec_rejects_unsafe_paths() {
        for path in [
            "",
            " src/**",
            "/src/**",
            "src//main.rs",
            "src/../secret",
            "src\nx",
        ] {
            assert!(
                normalize_pathspec(path, "read_path").is_err(),
                "expected {path:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_scope_hash_requires_64_hex_chars() {
        assert!(validate_scope_hash(&"f".repeat(64)).is_ok());
        assert!(validate_scope_hash("deadbeef").is_err());
        assert!(validate_scope_hash(&"z".repeat(64)).is_err());
    }

    #[tokio::test]
    async fn copy_lfs_objects_rehomes_visible_objects_under_view_prefix() {
        let store = Store::new(Arc::new(InMemory::new()));
        let source = LfsObjectStore::new(store.clone(), "org/repo");
        let view = LfsObjectStore::new(store.clone(), "org/repo/acl-views/v1/scope/view");
        let data = Bytes::from_static(b"hello");
        let oid: [u8; 32] = sha2::Sha256::digest(&data).into();
        source.put(&oid, data.clone()).await.unwrap();
        let pointer = LfsPointer {
            oid,
            size: data.len() as u64,
            extensions: Vec::new(),
        };

        copy_lfs_objects(
            store,
            "org/repo",
            "org/repo/acl-views/v1/scope/view",
            &[pointer],
        )
        .await
        .unwrap();

        assert_eq!(view.verify(&oid).await.unwrap(), data);
    }

    #[tokio::test]
    async fn build_filtered_view_keeps_allowed_crab_content_view_local() {
        let store = Store::new(Arc::new(InMemory::new()));
        let source_repo = "org/repo";
        let view_prefix = "org/repo/acl-views/v1/scope/1-deadbeef";
        let temp = tempfile::tempdir().unwrap();

        let content = b"allowed source content".to_vec();
        let file_hash = MerkleHash::from(*blake3::hash(&content).as_bytes());
        let mut chunker = GearChunker::new();
        let mut chunks = chunker.feed(&content);
        if let Some(last) = chunker.finalize() {
            chunks.push(last);
        }
        let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|chunk| chunk.hash).collect();
        let mut builder = XorbBuilder::new();
        for chunk in &chunks {
            builder.push(chunk, RunId(0)).unwrap();
        }
        let source_xorbs = builder.finalize().unwrap();
        let source_xorb_hash = source_xorbs[0].hash;
        let source_router = StoreLayout::new(store.clone(), source_repo.to_owned());
        let source_objects = upload_view_crab_objects(
            &store,
            &source_router,
            ViewCrabObjects {
                files: vec![RepackedFile {
                    file_hash,
                    size: content.len() as u64,
                    chunk_hashes,
                }],
                xorbs: source_xorbs,
            },
        )
        .await
        .unwrap();
        let source_registry_generation = crab_metadata::ref_registry::union_register_repo_shards(
            &store,
            &source_router,
            source_objects.shard_hashes.clone(),
        )
        .await
        .unwrap();
        let source_manifest = prepare_view_manifest(
            &store,
            &source_router,
            1,
            BTreeMap::new(),
            "refs/heads/main".to_owned(),
            BTreeMap::new(),
            source_objects.shard_hashes.clone(),
            None,
        )
        .await
        .unwrap();
        create_manifest(&store, &source_router, &source_manifest)
            .await
            .unwrap();
        commit_view_metadb(
            &store,
            &source_router,
            &source_objects,
            &source_manifest,
            source_registry_generation,
        )
        .await
        .unwrap();

        let pointer = Pointer {
            file_hash: file_hash.into(),
            size: content.len() as u64,
            shard_hint: None,
        };
        let work = temp.path().join("work");
        fs::create_dir_all(work.join("src")).unwrap();
        fs::create_dir_all(work.join("secret")).unwrap();
        run_git(["init", path_str(&work).unwrap()], None).unwrap();
        run_git(
            [
                "-C",
                path_str(&work).unwrap(),
                "config",
                "user.email",
                "view-test@example.com",
            ],
            None,
        )
        .unwrap();
        run_git(
            [
                "-C",
                path_str(&work).unwrap(),
                "config",
                "user.name",
                "View Test",
            ],
            None,
        )
        .unwrap();
        fs::write(work.join("src").join("allowed.bin"), pointer.serialize()).unwrap();
        fs::write(work.join("secret").join("hidden.txt"), b"denied").unwrap();
        run_git(["-C", path_str(&work).unwrap(), "add", "."], None).unwrap();
        run_git(
            ["-C", path_str(&work).unwrap(), "commit", "-m", "source"],
            None,
        )
        .unwrap();
        let source_bare = temp.path().join("source.git");
        run_git(
            [
                "clone",
                "--bare",
                path_str(&work).unwrap(),
                path_str(&source_bare).unwrap(),
            ],
            None,
        )
        .unwrap();

        build_filtered_view(
            path_str(&source_bare).unwrap(),
            source_repo,
            view_prefix,
            1,
            &store,
            &["src/**".to_owned()],
            &["secret/**".to_owned()],
            None,
        )
        .await
        .unwrap();

        let view_router = view_store_layout(&store, view_prefix);
        let (manifest, _) = read_manifest(&store, &view_router).await.unwrap();
        let visibility = crab_metadata::git_visibility::read(
            &store,
            &view_router,
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
        )
        .await
        .unwrap();
        assert!(manifest.refs.iter().all(|(name, tip)| {
            visibility
                .refs
                .get(name)
                .is_some_and(|objects| objects.binary_search(tip).is_ok())
        }));
        let packs = read_bulk_pack_list(&store, &view_router, &manifest.pack_index_hash)
            .await
            .unwrap();
        assert_eq!(packs.len(), 1);

        let pack_path = view_router.pack_path(&packs[0].pack_id);
        let (pack_bytes, _) = store.get_with_etag(&pack_path).await.unwrap();
        let view_git = temp.path().join("view.git");
        run_git(["init", "--bare", path_str(&view_git).unwrap()], None).unwrap();
        let pack_file = temp.path().join("view.pack");
        fs::write(&pack_file, &pack_bytes).unwrap();
        run_git_owned(
            vec![
                "--git-dir".to_owned(),
                path_str(&view_git).unwrap().to_owned(),
                "unpack-objects".to_owned(),
                "-q".to_owned(),
            ],
            None,
            None,
            Some(Stdio::from(File::open(&pack_file).unwrap())),
        )
        .unwrap();
        for (name, sha) in &manifest.refs {
            run_git(
                [
                    "--git-dir",
                    path_str(&view_git).unwrap(),
                    "update-ref",
                    name,
                    sha,
                ],
                None,
            )
            .unwrap();
        }

        let tree = run_git_capture(
            [
                "--git-dir",
                path_str(&view_git).unwrap(),
                "ls-tree",
                "-r",
                "--name-only",
                &manifest.head,
            ],
            None,
        )
        .unwrap();
        assert!(tree.contains("src/allowed.bin"));
        assert!(!tree.contains("secret/hidden.txt"));
        let log_names = run_git_capture(
            [
                "--git-dir",
                path_str(&view_git).unwrap(),
                "log",
                "--name-only",
                "--all",
                "--format=",
            ],
            None,
        )
        .unwrap();
        assert!(log_names.contains("src/allowed.bin"));
        assert!(!log_names.contains("secret/hidden.txt"));
        let objects = run_git_capture(
            [
                "--git-dir",
                path_str(&view_git).unwrap(),
                "rev-list",
                "--objects",
                "--all",
            ],
            None,
        )
        .unwrap();
        assert!(!objects.contains("secret/hidden.txt"));
        for oid in objects
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            let kind = run_git_capture(
                [
                    "--git-dir",
                    path_str(&view_git).unwrap(),
                    "cat-file",
                    "-t",
                    oid,
                ],
                None,
            )
            .unwrap();
            if kind.trim() != "blob" {
                continue;
            }
            let blob = run_git_capture(
                [
                    "--git-dir",
                    path_str(&view_git).unwrap(),
                    "cat-file",
                    "-p",
                    oid,
                ],
                None,
            )
            .unwrap();
            assert!(!blob.contains("denied"));
        }
        let allowed_blob_spec = format!("{}:src/allowed.bin", manifest.head);
        let blob = run_git_capture(
            [
                "--git-dir",
                path_str(&view_git).unwrap(),
                "cat-file",
                "-p",
                &allowed_blob_spec,
            ],
            None,
        )
        .unwrap();
        assert!(blob.contains("version https://crab.dev/spec/v1"));
        assert!(!blob.contains("allowed source content"));

        assert!(
            store
                .head(&ObjectPath::from(format!(
                    "{view_prefix}/.crab/xorbs/{}/{}",
                    &source_xorb_hash.hex()[..2],
                    source_xorb_hash.hex()
                )))
                .await
                .is_ok()
        );
        verify_crab_pointers_backed_by_view(&store, view_prefix, &[pointer])
            .await
            .unwrap();
    }
}
