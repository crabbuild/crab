use std::{
    fmt,
    fs::OpenOptions,
    path::{Path as FilePath, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use fs4::fs_std::FileExt;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions,
    Result, UpdateVersion, path::Path,
};

/// Test-only local object store with cross-process conditional updates.
///
/// `LocalFileSystem` deliberately does not implement `PutMode::Update`. This
/// adapter supplies the conditional control-record operation needed by the
/// process movement qualification while retaining the real local filesystem
/// and atomic object writes for every other operation.
#[derive(Clone, Debug)]
pub struct FilesystemCasStore {
    inner: Arc<object_store::local::LocalFileSystem>,
    root: Arc<PathBuf>,
}

impl FilesystemCasStore {
    pub fn new(root: &FilePath) -> Result<Self> {
        let inner = object_store::local::LocalFileSystem::new_with_prefix(root)?;
        std::fs::create_dir_all(root.join(".crab-cas-locks")).map_err(|source| {
            object_store::Error::Generic {
                store: "filesystem-cas-store",
                source: Box::new(source),
            }
        })?;
        Ok(Self {
            inner: Arc::new(inner),
            root: Arc::new(root.to_owned()),
        })
    }

    fn lock_path(&self, location: &Path) -> PathBuf {
        let digest = blake3::hash(location.as_ref().as_bytes());
        self.root
            .join(".crab-cas-locks")
            .join(format!("{}.lock", hex(digest.as_bytes())))
    }

    async fn update(
        &self,
        location: &Path,
        payload: PutPayload,
        mut options: PutOptions,
        expected: UpdateVersion,
    ) -> Result<PutResult> {
        let lock_path = self.lock_path(location);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|source| object_store::Error::Generic {
                store: "filesystem-cas-store",
                source: Box::new(source),
            })?;
        loop {
            if FileExt::try_lock_exclusive(&lock).map_err(|source| {
                object_store::Error::Generic {
                    store: "filesystem-cas-store",
                    source: Box::new(source),
                }
            })? {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let result = async {
            let current = self.inner.head(location).await?;
            let actual = UpdateVersion {
                e_tag: current.e_tag,
                version: current.version,
            };
            if actual != expected {
                return Err(object_store::Error::Precondition {
                    path: location.to_string(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "conditional filesystem update lost the race",
                    )),
                });
            }
            options.mode = PutMode::Overwrite;
            self.inner.put_opts(location, payload, options).await
        }
        .await;
        FileExt::unlock(&lock).map_err(|source| object_store::Error::Generic {
            store: "filesystem-cas-store",
            source: Box::new(source),
        })?;
        result
    }
}

#[async_trait]
impl ObjectStore for FilesystemCasStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult> {
        match options.mode.clone() {
            PutMode::Update(expected) => self.update(location, payload, options, expected).await,
            PutMode::Create | PutMode::Overwrite => {
                self.inner.put_opts(location, payload, options).await
            }
        }
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        self.inner.rename_opts(from, to, options).await
    }
}

impl fmt::Display for FilesystemCasStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("filesystem-cas-store")
    }
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(TABLE[(byte >> 4) as usize] as char);
        output.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    output
}
