use std::sync::Arc;

use bytes::Bytes;
use crab_sdk::remote::Snapshot;
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, GitPath, Repository, RepositoryLocator, Revision};
use crab_storage::{Store, StoreLayout};
use sha2::Digest as _;

use super::{capsule_fixture::CapsuleFixture, copy_published_objects, git};

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
    native: CapsuleFixture,
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
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "repository".to_owned());
        let mut native = CapsuleFixture::new(layout, &source, &pack, &commits).await;
        native
            .publish(
                std::collections::BTreeMap::from([(
                    "refs/heads/main".to_owned(),
                    commits[0].clone(),
                )]),
                Default::default(),
            )
            .await;
        #[cfg(feature = "content")]
        store
            .put(
                &crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &oid),
                updated.clone(),
            )
            .await
            .unwrap();
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
            native,
            second: commits[1].clone(),
        }
    }

    pub async fn advance(&mut self) -> Repository {
        self.native
            .publish(
                std::collections::BTreeMap::from([(
                    "refs/heads/main".to_owned(),
                    self.second.clone(),
                )]),
                Default::default(),
            )
            .await;
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
