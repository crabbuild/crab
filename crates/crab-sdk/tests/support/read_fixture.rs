use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocation, GitObjectLocatorEntry, GitPackLocatorRecord,
};
use crab_metadata::manifest_store::{
    create_manifest, read_manifest, upload_segmented_bulk, write_manifest_cas,
};
use crab_metadata::manifests::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
};
use crab_sdk::remote::Snapshot;
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, GitPath, Repository, RepositoryLocator, Revision};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use sha2::Digest as _;

use super::{copy_published_objects, git, publish_catalog};

pub struct ReadFixture {
    pub directory: tempfile::TempDir,
    pub client: Client,
    pub repository: Repository,
    pub snapshot: Snapshot,
    pub path: GitPath,
    pub original: Bytes,
    pub updated: Bytes,
    pub pointer: Bytes,
    pub store: Store,
    manifest: Manifest,
    coverage: GitLocatorCoverage,
    record: GitPackLocatorRecord,
    entries: Vec<GitObjectLocatorEntry>,
    second: String,
}

impl ReadFixture {
    pub async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let storage = directory.path().join("storage");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&storage).unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        #[cfg(unix)]
        let filename = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"file-\xff.bin".to_vec())
        };
        #[cfg(not(unix))]
        let filename = std::ffi::OsString::from("file.bin");
        let original = Bytes::from_static(b"first committed bytes\0\xff\n");
        let updated = Bytes::from(
            (0..192 * 1024)
                .map(|n| (n ^ (n >> 8)) as u8)
                .collect::<Vec<_>>(),
        );
        let oid: [u8; 32] = sha2::Sha256::digest(&updated).into();
        let pointer = Bytes::from(
            crab_git::LfsPointer {
                oid,
                size: updated.len() as u64,
                extensions: Vec::new(),
            }
            .serialize(),
        );
        std::fs::write(source.join("z-lfs"), &pointer).unwrap();
        let mut extended_pointer = crab_git::LfsPointer::parse(&pointer).unwrap();
        extended_pointer.extensions.push(crab_git::LfsExtension {
            name: "transform".to_owned(),
            priority: 1,
            oid: [0x5a; 32],
            oid_type: "sha256".to_owned(),
        });
        std::fs::write(source.join("z-lfs-extension"), extended_pointer.serialize()).unwrap();
        git(&source, &["add", "z-lfs", "z-lfs-extension"]);
        let mut commits = Vec::new();
        for (message, content) in [("first", &original), ("second", &updated)] {
            std::fs::write(source.join("payload"), content).unwrap();
            let blob = git(&source, &["hash-object", "-w", "payload"]);
            git(
                &source,
                &[
                    std::ffi::OsStr::new("update-index"),
                    std::ffi::OsStr::new("--add"),
                    std::ffi::OsStr::new("--cacheinfo"),
                    std::ffi::OsStr::new("100644"),
                    std::ffi::OsStr::new(&blob),
                    filename.as_os_str(),
                ],
            );
            git(&source, &["commit", "-m", message]);
            commits.push(git(&source, &["rev-parse", "HEAD"]));
        }
        let base = directory.path().join("fixture");
        let hash = git(
            &source,
            &[
                "pack-objects",
                "--all",
                "--index-version=2",
                base.to_str().unwrap(),
            ],
        );
        let pack = directory.path().join(format!("fixture-{hash}.pack"));
        let index = pack.with_extension("idx");
        let reverse = pack.with_extension("rev");
        crab_git::write_pack_reverse_index(&index, &reverse).unwrap();
        let pack_bytes = std::fs::read(&pack).unwrap();
        let pack_size = pack_bytes.len() as u64;
        let pack_id = MerkleHash::from_hex(blake3::hash(&pack_bytes).to_hex().as_str()).unwrap();
        let entries = crab_git::PackLocationIter::open(&index, &reverse, pack_size)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                GitObjectLocatorEntry {
                    oid: entry.oid.as_bytes().try_into().unwrap(),
                    location: GitObjectLocation {
                        pack_offset: entry.pack_offset,
                        entry_len: entry.entry_len,
                        crc32: entry.crc32,
                    },
                    metadata: Default::default(),
                }
            })
            .collect::<Vec<_>>();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "repository".to_owned());
        crab_metadata::layout_descriptor::ensure_canonical_layout(&store, &layout)
            .await
            .unwrap();
        for (path, bytes) in [
            (layout.pack_path(&pack_id), pack_bytes),
            (
                layout.pack_index_path(&pack_id),
                std::fs::read(index).unwrap(),
            ),
            (
                layout.pack_reverse_index_path(&pack_id),
                std::fs::read(reverse).unwrap(),
            ),
        ] {
            store.put(&path, Bytes::from(bytes)).await.unwrap();
        }
        #[cfg(feature = "content")]
        store
            .put(
                &crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &oid),
                updated.clone(),
            )
            .await
            .unwrap();
        let (pack_index_hash, _, pack_index) = compact_pack_index(
            1,
            &[PackManifestEntry {
                pack_id: pack_id.to_string(),
                content_hash: pack_id.to_string(),
                size: pack_size,
                object_count: entries.len() as u64,
                ref_tips: commits.clone(),
            }],
        )
        .unwrap();
        let (shard_index_hash, _, shard_index) = compact_shard_index(1, &[]).unwrap();
        upload_segmented_bulk(
            &store,
            &layout,
            &BulkData {
                pack_index,
                shard_index,
            },
        )
        .await
        .unwrap();
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest
            .refs
            .insert("refs/heads/main".into(), commits[0].clone());
        manifest.pack_index_hash = pack_index_hash;
        manifest.shard_index_hash = shard_index_hash;
        manifest.seal_git_validation();
        create_manifest(&store, &layout, &manifest).await.unwrap();
        let coverage = GitLocatorCoverage {
            generation: 1,
            pack_index_hash: MerkleHash::from_hex(&manifest.pack_index_hash).unwrap(),
        };
        let record = GitPackLocatorRecord {
            pack_id,
            committed_generation: 1,
            pack_index_hash: coverage.pack_index_hash,
            object_count: entries.len() as u64,
            pack_size,
        };
        publish_catalog(&store, coverage, record, &entries).await;
        copy_published_objects(&store, &storage, None, &mut Default::default()).await;
        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_file(pack).unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::filesystem(&storage).unwrap())
            .build()
            .unwrap();
        let repository = client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap(),
            ))
            .await
            .unwrap();
        let snapshot = repository
            .remote()
            .unwrap()
            .snapshot(Revision::branch("main").unwrap())
            .await
            .unwrap();
        let path = GitPath::new(filename.as_encoded_bytes().to_vec()).unwrap();
        Self {
            directory,
            client,
            repository,
            snapshot,
            path,
            original,
            updated,
            pointer,
            store,
            manifest,
            coverage,
            record,
            entries,
            second: commits[1].clone(),
        }
    }

    pub async fn advance(&mut self) -> Repository {
        let layout = StoreLayout::new(self.store.clone(), "repository".to_owned());
        let (_, etag) = read_manifest(&self.store, &layout).await.unwrap();
        self.manifest.generation += 1;
        self.manifest
            .refs
            .insert("refs/heads/main".into(), self.second.clone());
        self.manifest.seal_git_validation();
        write_manifest_cas(&self.store, &layout, &self.manifest, &etag)
            .await
            .unwrap();
        self.coverage.generation = self.manifest.generation;
        publish_catalog(&self.store, self.coverage, self.record, &self.entries).await;
        copy_published_objects(
            &self.store,
            &self.directory.path().join("storage"),
            None,
            &mut Default::default(),
        )
        .await;
        self.repository.remote().unwrap().refresh().await.unwrap()
    }
}
