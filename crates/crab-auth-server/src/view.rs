use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crab_cache_store::{CacheConfig, CachingStore};
use crab_git::{CrabUrl, lfs_pointer::LfsPointer};
use crab_lfs::LfsObjectStore;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_read::ShardHydrator;
use crab_storage::{StorageProviderKind, Store, build_static_env_store};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::ShardReader;
use crab_xet::shard_parse::MAX_SHARD_SIZE_BYTES;
use serde::Serialize;

use crate::error::{AuthServerError, Result};
use crate::git_pointer_scan::scan_reachable_pointers;

mod capsule;
mod git_workspace;
mod objects;
mod repack;

use capsule::{
    publish as publish_filtered_capsule_view, verify_ready as verify_capsule_view_ready,
};
use git_workspace::{
    ViewGitWorkspace, generate_view_pack, list_view_refs, materialize_bare, resolve_view_head,
};
use objects::upload_view_crab_objects;
use repack::{ViewCrabObjects, ViewCrabRepacker, materialize_crab_pointers_in_fast_export};

#[cfg(test)]
use git_workspace::{path_str, run_git, run_git_capture};

type StoreLayout = crab_storage::StoreLayout<Store>;

struct SourceViewIdentity {
    view: crab_read::capsule_protocol::CapsuleRepositoryView,
    generation: u64,
    digest: String,
    lookup: SharedFileIndexLookup,
}

async fn read_source_view_identity(router: &StoreLayout) -> Result<SourceViewIdentity> {
    let root = crab_metadata::capsule_protocol::load_root(router).await?;
    let view = crab_read::capsule_protocol::open_view_from_root_with_control(
        router,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let catalog = view.pointer_catalog_lookup(router, 2 * 1024 * 1024 * 1024)?;
    let lookup = SharedFileIndexLookup::for_pointer_catalog(router.clone(), catalog);
    Ok(SourceViewIdentity {
        generation: view.root().root().generation(),
        digest: view.state_digest(),
        view,
        lookup,
    })
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
    let source = read_source_view_identity(&source_router).await?;
    let result = async {
        let repo_prefix = view_prefix(
            &parsed.repo_path,
            scope_hash,
            source.generation,
            &source.digest,
        );
        let global_prefix = format!("{repo_prefix}/.crab");
        let output = ViewOutput {
            repo_prefix: repo_prefix.clone(),
            global_prefix,
            source_repo: parsed.repo_path.clone(),
            scope_hash: scope_hash.to_ascii_lowercase(),
            source_generation: source.generation,
            source_manifest_hash: source.digest.clone(),
            cache_hit: false,
        };

        let view_router = view_store_layout(&store, &repo_prefix);
        let cached = verify_capsule_view_ready(&view_router, &source.digest).await;
        match cached {
            Ok(()) => {
                verify_existing_view(&view_router).await?;
                return Ok(ViewOutput {
                    cache_hit: true,
                    ..output
                });
            }
            Err(AuthServerError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }

        build_filtered_view(&source_router, &repo_prefix, &source, &include, &deny).await?;
        verify_capsule_view_ready(&view_router, &source.digest).await?;

        Ok(output)
    }
    .await;
    let close = source.lookup.close().await;
    if let (Err(_), Err(error)) = (&result, &close) {
        tracing::warn!(%error, "view failed; source lookup cleanup also failed");
    }
    let output = result?;
    close?;
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
    source_router: &StoreLayout,
    repo_prefix: &str,
    source: &SourceViewIdentity,
    include: &[String],
    deny: &[String],
) -> Result<()> {
    // Git and pointer recipes must use the same captured source. Cloning latest
    // here can publish later history under an earlier view/cache identity.
    let workspace = ViewGitWorkspace::create(source_router, &source.view, include, deny).await?;
    let store = source_router.store();
    let source_repo = source_router.repo_prefix();
    let hydrator = source_hydrator(source_router.clone(), workspace.temp_path())?
        .with_file_index_lookup(source.lookup.clone());
    let mut repacker = ViewCrabRepacker::new(hydrator);
    materialize_crab_pointers_in_fast_export(
        workspace.export_stream(),
        workspace.repacked_stream(),
        &mut repacker,
    )
    .await?;
    workspace.import_repacked_history()?;
    workspace.validate_git_state()?;
    let crab_objects = repacker.finish()?;
    publish_filtered_capsule_view(
        source_repo,
        repo_prefix,
        &source.digest,
        store,
        workspace.filtered_git(),
        crab_objects,
    )
    .await?;
    verify_filtered_view_content(workspace.filtered_git(), store, source_repo, repo_prefix).await
}

fn source_hydrator(router: StoreLayout, temp_dir: &Path) -> Result<ShardHydrator> {
    let cache = Arc::new(crab_cache::LocalCache::new(temp_dir.join("hydrate-cache")));
    let caching =
        CachingStore::new_with_local_cache(router.store().clone(), CacheConfig::default(), cache)?;
    crab_read::ReadRuntimeBuilder::new(caching, router, 16)
        .build()
        .map_err(AuthServerError::from)
}

async fn verify_existing_view(router: &StoreLayout) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let view_git = temp.path().join("view.git");
    let source = read_source_view_identity(router).await?;
    let result = async {
        materialize_bare(router, &source.view, &view_git).await?;
        let scan = scan_reachable_pointers(&view_git)?;
        verify_crab_pointers_backed_by_view(router, &source.lookup, &scan.crab_pointers).await?;
        let lfs = LfsObjectStore::new(router.store().clone(), router.repo_prefix());
        for pointer in scan.lfs_pointers {
            lfs.verify(&pointer.oid).await?;
        }
        Ok::<_, AuthServerError>(())
    }
    .await;
    let close = source.lookup.close().await;
    result?;
    close?;
    Ok(())
}

async fn verify_filtered_view_content(
    filtered_git: &Path,
    store: &Store,
    source_repo: &str,
    repo_prefix: &str,
) -> Result<()> {
    let scan = scan_reachable_pointers(filtered_git)?;
    let router = view_store_layout(store, repo_prefix);
    let source = read_source_view_identity(&router).await?;
    let result = async {
        verify_crab_pointers_backed_by_view(&router, &source.lookup, &scan.crab_pointers).await?;
        copy_lfs_objects(store.clone(), source_repo, repo_prefix, &scan.lfs_pointers).await
    }
    .await;
    let close = source.lookup.close().await;
    result?;
    close?;
    Ok(())
}

async fn verify_crab_pointers_backed_by_view(
    router: &StoreLayout,
    lookup: &SharedFileIndexLookup,
    pointers: &[Pointer],
) -> Result<()> {
    let mut seen = HashSet::new();
    for pointer in pointers {
        let file_hash = MerkleHash::from(pointer.file_hash);
        if !seen.insert(file_hash) {
            continue;
        }
        let shard_hash =
            lookup
                .lookup(&file_hash)
                .await?
                .ok_or_else(|| AuthServerError::AuthFailed {
                    path: format!(
                        "filtered ACL view pointer {} has no view-local file-index entry",
                        file_hash.hex()
                    ),
                })?;
        let (shard_bytes, _) = router
            .store()
            .get_with_etag_bounded(&router.shard_path(&shard_hash), MAX_SHARD_SIZE_BYTES as u64)
            .await?;
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
            router
                .store()
                .head(&router.xorb_path(&segment.xorb_hash))
                .await?;
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
            let (bytes, _) = store
                .get_with_etag_bounded(&path, MAX_SHARD_SIZE_BYTES as u64)
                .await?;
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

fn view_store_layout(store: &Store, repo_prefix: &str) -> StoreLayout {
    StoreLayout::with_global_prefix(
        store.clone(),
        repo_prefix.to_owned(),
        format!("{}/.crab", repo_prefix.trim_end_matches('/')),
    )
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
    source_digest: &str,
) -> String {
    format!(
        "{}/acl-views/v2/{}/{}-{}",
        source_repo.trim_matches('/'),
        scope_hash.to_ascii_lowercase(),
        generation,
        source_digest
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use bytes::Bytes;
    use crab_xet::chunker::GearChunker;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use sha2::Digest;

    use super::repack::RepackedFile;
    use super::*;

    #[tokio::test]
    async fn manifest_source_cannot_authorize_view() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &router)
            .await
            .unwrap();
        crab_metadata::manifest_store::create_manifest(
            &store,
            &router,
            &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();
        let result = read_source_view_identity(&router).await;
        let refused = matches!(result, Err(AuthServerError::NotFound { ref path }) if path == router.capsule_root_path().as_ref());
        if let Ok(source) = result {
            source.lookup.close().await.unwrap();
        }
        assert!(
            refused,
            "a valid manifest cannot replace Capsule source authority"
        );
    }

    #[test]
    fn view_prefix_includes_source_scope_generation_and_digest() {
        let prefix = view_prefix("org/repo", &"A".repeat(64), 7, "deadbeef");

        assert_eq!(
            prefix,
            format!("org/repo/acl-views/v2/{}/7-deadbeef", "a".repeat(64))
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
        let view = LfsObjectStore::new(store.clone(), "org/repo/acl-views/v2/scope/view");
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
            "org/repo/acl-views/v2/scope/view",
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
        let view_prefix = "org/repo/acl-views/v2/scope/1-deadbeef";
        let temp = tempfile::tempdir().unwrap();

        let content = b"allowed source content".repeat(65_537);
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
        let source_router = view_store_layout(&store, source_repo);
        let source_objects = ViewCrabObjects {
            files: vec![RepackedFile {
                file_hash,
                size: content.len() as u64,
                chunk_hashes,
            }],
            xorbs: source_xorbs,
        };

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

        publish_filtered_capsule_view(
            source_repo,
            source_repo,
            &"b".repeat(64),
            &store,
            &source_bare,
            source_objects,
        )
        .await
        .unwrap();
        let source = read_source_view_identity(&source_router).await.unwrap();
        store
            .delete(&source_router.capsule_root_path())
            .await
            .unwrap();
        build_filtered_view(
            &source_router,
            view_prefix,
            &source,
            &["src/**".to_owned()],
            &["secret/**".to_owned()],
        )
        .await
        .unwrap();
        source.lookup.close().await.unwrap();

        let view_router = view_store_layout(&store, view_prefix);
        let view = read_source_view_identity(&view_router).await.unwrap();
        let visibility = view.view.git_visibility_index().await.unwrap();
        assert!(
            view.view
                .refs()
                .iter()
                .all(|(name, tip)| visibility.contains_hex_in_ref(name, tip))
        );
        let view_git = temp.path().join("view.git");
        materialize_bare(&view_router, &view.view, &view_git)
            .await
            .unwrap();

        let tree = run_git_capture(
            [
                "--git-dir",
                path_str(&view_git).unwrap(),
                "ls-tree",
                "-r",
                "--name-only",
                view.view.head(),
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
        let allowed_blob_spec = format!("{}:src/allowed.bin", view.view.head());
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
        assert!(blob.contains("version https://crab.build/spec/v1"));
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
        verify_crab_pointers_backed_by_view(
            &view_router,
            &view.lookup,
            std::slice::from_ref(&pointer),
        )
        .await
        .unwrap();
        let cache = Arc::new(crab_cache::LocalCache::new(
            temp.path().join("view-read-cache"),
        ));
        let caching =
            CachingStore::new_with_local_cache(store, CacheConfig::default(), cache).unwrap();
        let lookup = view.lookup;
        let reader = crab_read::ReadRuntimeBuilder::new(caching, view_router, 2)
            .with_file_index_lookup(lookup.clone())
            .build()
            .unwrap();
        assert_eq!(
            reader
                .reconstruct_from_pointer(&pointer.serialize())
                .await
                .unwrap(),
            content
        );
        lookup.close().await.unwrap();
    }

    #[tokio::test]
    async fn capsule_view_publication_is_readable_and_retry_safe() {
        let store = Store::new(Arc::new(InMemory::new()));
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        run_git(["init", "-b", "main", path_str(&work).unwrap()], None).unwrap();
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
        fs::write(work.join("allowed.txt"), b"visible").unwrap();
        run_git(["-C", path_str(&work).unwrap(), "add", "."], None).unwrap();
        run_git(
            ["-C", path_str(&work).unwrap(), "commit", "-m", "visible"],
            None,
        )
        .unwrap();
        let filtered_git = temp.path().join("filtered.git");
        run_git(
            [
                "clone",
                "--bare",
                path_str(&work).unwrap(),
                path_str(&filtered_git).unwrap(),
            ],
            None,
        )
        .unwrap();

        let repo_prefix = "org/repo/acl-views/v2/scope/view";
        let source_digest = "a".repeat(64);
        for _ in 0..2 {
            publish_filtered_capsule_view(
                "org/repo",
                repo_prefix,
                &source_digest,
                &store,
                &filtered_git,
                ViewCrabObjects {
                    files: Vec::new(),
                    xorbs: Vec::new(),
                },
            )
            .await
            .unwrap();
        }

        let source_url = format!("crab://bucket/{repo_prefix}");
        let scope = "c".repeat(64);
        let first = materialize_view_with_store(
            &source_url,
            &scope,
            &["allowed.txt".to_owned()],
            &[],
            store.clone(),
        )
        .await
        .unwrap();
        let second = materialize_view_with_store(
            &source_url,
            &scope,
            &["allowed.txt".to_owned()],
            &[],
            store.clone(),
        )
        .await
        .unwrap();
        assert!(!first.cache_hit && second.cache_hit);
        assert_eq!(first.repo_prefix, second.repo_prefix);
        verify_existing_view(&view_store_layout(&store, &second.repo_prefix))
            .await
            .unwrap();

        let router = view_store_layout(&store, repo_prefix);
        assert!(
            store.head(&router.layout_descriptor_path()).await.is_err(),
            "capsule view publication must not create legacy layout metadata"
        );
        verify_capsule_view_ready(&router, &source_digest)
            .await
            .unwrap();
        let view = crab_read::capsule_protocol::open_view(
            &router,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: u64::MAX,
                max_frontier_bytes: u64::MAX,
            },
        )
        .await
        .unwrap();
        let expected =
            run_git_capture(["-C", path_str(&work).unwrap(), "rev-parse", "HEAD"], None).unwrap();
        assert_eq!(
            view.refs().get("refs/heads/main").map(String::as_str),
            Some(expected.trim())
        );
        assert!(
            view.git_visibility_index()
                .await
                .unwrap()
                .contains_hex_in_ref("refs/heads/main", expected.trim())
        );
    }
}
