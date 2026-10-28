use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use crab_cache::LocalCache;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_read::ReadRuntimeBuilder;
use crab_storage::Store;
use crab_storage::test_support::CountingObjectStore;
use crab_types::pointer::Pointer;
use crab_xet::reconstruction::ChunkPlacementMap;
use crab_xet::shard::{ShardWriter, file_info_from_placements, xorb_info_from_placements};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use object_store::memory::InMemory;

use crate::MountReadContext;

struct FixedCatalog(Arc<crab_read::ShardHydrator>);

#[async_trait::async_trait]
impl crate::MountCatalogCapture for FixedCatalog {
    async fn shutdown(&self) -> crate::Result<()> {
        Ok(())
    }

    async fn capture(&self) -> crate::Result<Arc<crab_read::ShardHydrator>> {
        Ok(Arc::clone(&self.0))
    }
}

pub(crate) struct StoredPointer {
    pub(crate) context: MountReadContext,
    pub(crate) pointer: Pointer,
    pub(crate) origin: Arc<CountingObjectStore>,
    pub(crate) xorb_path: object_store::path::Path,
}

impl StoredPointer {
    #[expect(
        clippy::unwrap_used,
        reason = "construct encoded captured catalog fixture"
    )]
    pub(crate) async fn with_catalog(mut self) -> Self {
        use crab_metadata::capsule_protocol::{
            CapsuleGitPack, FileCatalogEntry, LayeredCheckpoint, PackLayer, PointerCatalog,
            ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
        };
        let shard_hash = crab_xet::hash::MerkleHash::from(self.pointer.shard_hint.unwrap());
        let layout = &self.context.store_layout;
        let (shard, _) = layout
            .store()
            .get_with_etag(&layout.shard_path(&shard_hash))
            .await
            .unwrap();
        let mut chunks = std::collections::BTreeMap::<_, Vec<_>>::new();
        crab_xet::shard_parse::visit_xorb_chunks_from_reader(
            &mut std::io::Cursor::new(&shard),
            |xorb, _, chunk, size| {
                chunks
                    .entry(xorb)
                    .or_default()
                    .push(XorbChunkEntry::new(chunk.hex(), size));
                Ok(())
            },
        )
        .unwrap();
        let mut catalog = PointerCatalog::new();
        catalog
            .insert_file(
                crab_xet::hash::MerkleHash::from(self.pointer.file_hash).hex(),
                FileCatalogEntry::new(self.pointer.size, shard_hash.hex()),
            )
            .unwrap();
        catalog
            .insert_shard(
                shard_hash.hex(),
                ShardCatalogEntry::new(
                    shard.len() as u64,
                    chunks.keys().map(crab_xet::hash::MerkleHash::hex).collect(),
                ),
            )
            .unwrap();
        for (hash, entries) in chunks {
            let (body, _) = layout
                .store()
                .get_with_etag(&layout.xorb_path(&hash))
                .await
                .unwrap();
            catalog
                .insert_xorb(
                    hash.hex(),
                    XorbCatalogEntry::new(
                        body.len() as u64,
                        blake3::hash(&body).to_hex().to_string(),
                        entries,
                    ),
                )
                .unwrap();
        }
        // Only catalog pages are exercised here. Complete Git/control capture
        // and mounted origin behavior require the separate RustFS qualification.
        let pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "1".repeat(40),
            1,
        )
        .unwrap();
        let checkpoint = LayeredCheckpoint::build(
            1,
            &"3".repeat(64),
            vec![
                PackLayer::build(&pack)
                    .unwrap()
                    .source_descriptor()
                    .unwrap(),
            ],
            catalog,
            None,
        )
        .unwrap();
        checkpoint.publish_catalog(layout).await.unwrap();
        let lookup = checkpoint
            .pointer_catalog_lookup(layout, 1024 * 1024)
            .unwrap();
        let lookup = crab_metadata::file_index_lookup::SharedFileIndexLookup::for_pointer_catalog(
            layout.clone(),
            lookup,
        );
        let hydrator = Arc::new(
            self.context
                .hydrator
                .as_ref()
                .clone()
                .with_file_index_lookup(lookup),
        );
        self.context.hydrator = Arc::clone(&hydrator);
        self.context.catalog = Arc::new(FixedCatalog(hydrator));
        self.pointer.shard_hint = None;
        self
    }

    pub(crate) fn xorb_body_requests(&self) -> usize {
        use crab_storage::test_support::ObjectReadKind;

        let path = self.xorb_path.to_string();
        self.origin
            .requests()
            .iter()
            .filter(|request| request.location == path && request.kind != ObjectReadKind::Head)
            .count()
    }

    #[expect(clippy::unwrap_used, reason = "construct real shard/xorb test fixture")]
    pub(crate) async fn new(cache_root: &Path, content: Bytes) -> Self {
        let file_hash = *blake3::hash(&content).as_bytes();
        let size = u32::try_from(content.len()).unwrap();
        let mut builder = XorbBuilder::new();
        let mut recipe = Vec::new();
        for bytes in content.chunks(64 * 1024) {
            let chunk = crab_xet::xorb::format::Chunk::new(Bytes::copy_from_slice(bytes));
            recipe.push(chunk.hash);
            builder.push(&chunk, RunId(0)).unwrap();
        }
        let mut xorbs = builder.finalize().unwrap();
        assert_eq!(xorbs.len(), 1, "fixture fits one xorb");
        let xorb = xorbs.remove(0);
        let placements = xorb
            .placements
            .iter()
            .map(|placement| (placement.chunk_hash, placement.clone()))
            .collect::<ChunkPlacementMap>();
        let mut shard = ShardWriter::new();
        shard
            .add_xorb(Arc::new(
                xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap(),
            ))
            .unwrap();
        shard
            .add_file(file_info_from_placements(file_hash.into(), &recipe, &placements).unwrap())
            .unwrap();
        let (shard_bytes, shard_hash) = shard.finalize().unwrap();
        let origin = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
        let store = Store::new(origin.clone());
        let layout = crate::StoreLayout::new(store.clone(), "vfs-read-test".into());
        let xorb_path = layout.xorb_path(&xorb.hash);
        store.put(&xorb_path, xorb.bytes).await.unwrap();
        store
            .put(&layout.shard_path(&shard_hash), shard_bytes.into())
            .await
            .unwrap();
        let local = Arc::new(LocalCache::new(cache_root.to_owned()));
        let caching =
            CachingStore::new_with_local_cache(store, CacheConfig::default(), local).unwrap();
        let hydrator = ReadRuntimeBuilder::new(caching, layout.clone(), 2)
            .build()
            .unwrap();
        let hydrator = Arc::new(hydrator);
        Self {
            context: MountReadContext {
                store_layout: layout,
                hydrator: Arc::clone(&hydrator),
                catalog: Arc::new(FixedCatalog(hydrator)),
            },
            pointer: Pointer {
                file_hash,
                size: u64::from(size),
                shard_hint: Some(shard_hash.into()),
            },
            origin,
            xorb_path,
        }
    }
}

#[expect(clippy::unwrap_used, reason = "Git fixture setup")]
pub(crate) fn create_git_repo_with_blob(content: &[u8]) -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let git_dir = dir.path().join("test.git");

    // git init --bare
    let output = std::process::Command::new("git")
        .args(["init", "--bare"])
        .arg(&git_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git init failed");

    let oid_hex = write_git_blob(&git_dir, content);
    (dir, git_dir, oid_hex)
}

#[expect(clippy::unwrap_used, reason = "Git fixture setup")]
pub(crate) fn write_git_blob(git_dir: &Path, content: &[u8]) -> String {
    let output = std::process::Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_DIR", git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(content).unwrap();
            child.wait_with_output()
        })
        .unwrap();
    assert!(output.status.success(), "git hash-object failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[expect(clippy::unwrap_used, reason = "Git fixture setup")]
pub(crate) fn pack_git_blobs(git_dir: &Path, oids: &[&str]) {
    use std::io::Write;
    let prefix = git_dir.join("objects/pack/pack");
    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("pack-objects")
        .arg(prefix)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for oid in oids {
        writeln!(input, "{oid}").unwrap();
    }
    drop(input);
    assert!(child.wait().unwrap().success());
}
