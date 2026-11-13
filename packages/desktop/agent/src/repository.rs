//! Desktop browse sessions: local Git indexes and native v2 SDK snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use crab_sdk::remote::{ContentKind, EntryMode, PageRequest, Snapshot};
use crab_sdk::storage::{ContentCache, DirectStoreOptions};
use crab_sdk::{Client, GitPath, OpenOptions, RepositoryLocator, Revision};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::{AgentError, Result};

mod content;
mod files;
mod native;
pub(crate) use content::command::output as git_output;
pub(crate) use content::diff;
pub(crate) use content::{
    Dehydration, FileSource, FileVersion, MAX_COMPLETE_BYTES, ReadScope, Restoration,
};

#[derive(Clone)]
pub struct BrowseSession {
    pub url: String,
    pub work_dir: Option<PathBuf>,
    remote: Option<Arc<RemoteSession>>,
    readers: native::NativeReads,
    pub(crate) files: files::Files,
}

struct RemoteSession {
    client: Client,
    repository: crab_sdk::Repository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    Submodule,
    File,
    Pointer,
    Lfs,
}

pub struct BrowserEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub pointer_hash: Option<String>,
}

pub struct FileMetadata {
    pub size: u64,
    pub pointer_hash: Option<String>,
    pub mtime: Option<SystemTime>,
    pub rev: Option<String>,
    pub kind: ContentKind,
}

pub fn token_cache_directory() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| AgentError::Other("cannot determine home directory".into()))?;
    let configuration = home.join(".config/crab/config.toml");
    let text = match std::fs::read_to_string(configuration) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let config: toml::Value = toml::from_str(&text)?;
    let path = config
        .get("auth")
        .and_then(|auth| auth.get("token_cache_path"))
        .and_then(toml::Value::as_str)
        .unwrap_or("~/.config/crab/tokens/");
    Ok(crab_auth::token_cache::expand_token_cache_path(path))
}

impl BrowseSession {
    pub(crate) fn stop_reads(&self) {
        self.readers.cancel();
    }

    pub(crate) fn read_scope(&self) -> ReadScope {
        ReadScope::new(self.readers.read_token())
    }

    pub(crate) async fn open_local(path: &Path, scope: &ReadScope) -> Result<Self> {
        let root = content::command::run(path, &["rev-parse", "--show-toplevel"], scope).await?;
        let root = PathBuf::from(String::from_utf8(root)?.trim());
        let origin = git_output(&root, &["remote", "get-url", "origin"], scope).await?;
        let url = if origin.status.success() {
            String::from_utf8(origin.stdout)?.trim().to_owned()
        } else {
            root.display().to_string()
        };
        Ok(Self {
            url,
            work_dir: Some(root),
            remote: None,
            readers: native::NativeReads::default(),
            files: files::Files::default(),
        })
    }

    pub(crate) async fn open_url(url: &str, scope: &ReadScope) -> Result<Self> {
        scope.check()?;
        let cache = token_cache_directory()?;
        let managed = crab_sdk::managed::Options::new(&cache)?;
        let content_root = crab_cache::default_cache_root();
        // Remote reconstruction uses the same product root inspected and cleaned
        // by Settings; each storage identity retains a separate SDK namespace.
        let content = ContentCache::new(&content_root, None)?;
        let mut builder = Client::builder().managed(managed).content_cache(content);
        let locator = if let Some(connection) = crate::storage_auth::current() {
            builder = builder.direct_store(connection.options()?);
            RepositoryLocator::new(&connection.repository_prefix(url)?)?
        } else if url.starts_with("crab://") {
            let resolver = crab_auth_store::ManagedRepositoryResolver::new(cache);
            match resolver
                .classify(url)
                .map_err(crate::managed_service::managed_repository_error)?
            {
                crab_git::RepositoryLocator::Managed(repo) => RepositoryLocator::managed(
                    &repo.authority,
                    &repo.organization,
                    &repo.repository,
                )?,
                crab_git::RepositoryLocator::Direct(repo) => {
                    let options = match crab_storage::resolve_static_env_provider()? {
                        crab_storage::StorageProviderKind::S3 => {
                            DirectStoreOptions::s3_from_env(&repo.bucket)?
                        }
                        crab_storage::StorageProviderKind::Gcs => {
                            DirectStoreOptions::gcs_from_env(&repo.bucket)?
                        }
                        crab_storage::StorageProviderKind::Azure => {
                            let account = std::env::var("AZURE_STORAGE_ACCOUNT_NAME").map_err(|_| AgentError::Other("AZURE_STORAGE_ACCOUNT_NAME is required for default Azure credentials".into()))?;
                            DirectStoreOptions::azure_from_env(&account, &repo.bucket)?
                        }
                        crab_storage::StorageProviderKind::Local => {
                            return Err(AgentError::Other(
                                "Local storage is not a cloud repository provider".into(),
                            ));
                        }
                    };
                    builder = builder.direct_store(options);
                    RepositoryLocator::new(&repo.repo_prefix)?
                }
            }
        } else {
            let parsed = crab_git::RepositoryUrl::parse(url)?;
            let scheme = url.split_once("://").map(|p| p.0).unwrap_or_default();
            let mut prefix = parsed.repo_prefix.clone();
            let store = match scheme {
                "s3" => DirectStoreOptions::s3_from_env(&parsed.bucket)?,
                "gs" | "gcs" => DirectStoreOptions::gcs_from_env(&parsed.bucket)?,
                "az" | "azure" => {
                    let (container, repository) =
                        parsed.repo_prefix.split_once('/').ok_or_else(|| {
                            AgentError::Other(
                                "Azure URLs require an account, container, and repository".into(),
                            )
                        })?;
                    prefix = repository.to_owned();
                    DirectStoreOptions::azure_from_env(&parsed.bucket, container)?
                }
                _ => {
                    return Err(AgentError::Other(
                        "unsupported remote repository scheme".into(),
                    ));
                }
            };
            builder = builder.direct_store(store);
            RepositoryLocator::new(&prefix)?
        };
        let client = builder.build()?;
        let repository = match client
            .open(OpenOptions::remote(locator))
            .with_options(scope.options())
            .await
        {
            Ok(repository) => repository,
            Err(error) => {
                return match client.close().await {
                    Ok(()) => Err(error.into()),
                    Err(cleanup) => Err(AgentError::Cleanup {
                        operation: Box::new(error.into()),
                        cleanup: Box::new(cleanup.into()),
                    }),
                };
            }
        };
        Ok(Self {
            url: url.to_owned(),
            work_dir: None,
            remote: Some(Arc::new(RemoteSession { client, repository })),
            readers: native::NativeReads::default(),
            files: files::Files::default(),
        })
    }

    pub async fn close(&self) -> Result<()> {
        // Materialization owns a repository scope even after its view closes.
        // Stop it before waiting for file guards, otherwise close can deadlock.
        self.stop_reads();
        let files = self.files.close().await;
        let native = self.readers.close().await;
        let remote = match &self.remote {
            Some(remote) => remote.client.close().await.map_err(AgentError::from),
            None => Ok(()),
        };
        files.and(native).and(remote)
    }

    pub async fn refs(&self) -> Result<crab_sdk::remote::References> {
        let remote = self
            .remote
            .as_ref()
            .ok_or_else(|| AgentError::Other("remote session required".into()))?;
        Ok(remote.repository.remote()?.refs().await?)
    }

    async fn snapshot(
        &self,
        rev: Option<&str>,
        options: crab_sdk::operation::Options,
    ) -> Result<Snapshot> {
        let remote = self
            .remote
            .as_ref()
            .ok_or_else(|| AgentError::Other("remote session required".into()))?;
        let repository = remote.repository.remote()?;
        let revision = match rev {
            None | Some("HEAD") => {
                let refs = repository.refs().with_options(options.clone()).await?;
                revision(
                    refs.head()
                        .ok_or_else(|| AgentError::Other("remote has no HEAD".into()))?,
                )?
            }
            Some(rev) => revision(rev)?,
        };
        Ok(repository.snapshot(revision).with_options(options).await?)
    }

    pub async fn list(&self, prefix: Option<&str>, rev: Option<&str>) -> Result<Vec<BrowserEntry>> {
        let mut entries = Vec::new();
        if let Some(root) = &self.work_dir {
            let output = if let Some(rev) = rev {
                git(root, &["ls-tree", "-r", "-t", "-z", "--long", rev, "--"]).await?
            } else {
                git(root, &["ls-files", "--stage", "-z"]).await?
            };
            let mut blobs = Vec::new();
            for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                let Some(tab) = record.iter().position(|b| *b == b'\t') else {
                    continue;
                };
                let path = String::from_utf8(record[tab + 1..].to_vec())?;
                if prefix.is_some_and(|prefix| !path.starts_with(prefix)) {
                    continue;
                }
                let header = String::from_utf8(record[..tab].to_vec())?;
                let fields: Vec<_> = header.split_whitespace().collect();
                if fields.len() < 3 {
                    return Err(AgentError::Other("invalid Git file listing".into()));
                }
                let mode = fields[0];
                let oid = if rev.is_some() { fields[2] } else { fields[1] };
                let kind = match mode {
                    "040000" => EntryKind::Dir,
                    "160000" => EntryKind::Submodule,
                    _ => EntryKind::File,
                };
                if matches!(kind, EntryKind::Dir | EntryKind::Submodule) {
                    entries.push(BrowserEntry {
                        path: path.into(),
                        kind,
                        size: None,
                        pointer_hash: None,
                    });
                    continue;
                }
                blobs.push((path, oid.to_owned()));
            }
            // Batch sizes and small pointer bodies; large repositories must not spawn per-file Git processes.
            for chunk in blobs.chunks(1000) {
                let oids: Vec<_> = chunk.iter().map(|(_, oid)| oid.as_str()).collect();
                let headers = String::from_utf8(git_batch(root, "--batch-check", &oids).await?)?;
                let headers: Vec<_> = headers.lines().collect();
                if headers.len() != chunk.len() {
                    return Err(AgentError::Other("incomplete Git object inventory".into()));
                }
                let mut small = Vec::new();
                let start = entries.len();
                for ((name, oid), header) in chunk.iter().zip(headers) {
                    let fields: Vec<_> = header.split_whitespace().collect();
                    if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
                        return Err(AgentError::Other(
                            "Git object inventory does not match the index".into(),
                        ));
                    }
                    let size = fields[2]
                        .parse::<u64>()
                        .map_err(|_| AgentError::Other("invalid Git blob size".into()))?;
                    if size < 1024 {
                        small.push((entries.len(), oid.as_str()));
                    }
                    entries.push(entry(name.clone(), size, &[]));
                }
                if !small.is_empty() {
                    let selected: Vec<_> = small.iter().map(|(_, oid)| *oid).collect();
                    let output = git_batch(root, "--batch", &selected).await?;
                    crab_git::batch::visit_small_blobs(
                        output.as_slice(),
                        selected.iter().copied(),
                        1024,
                        &|| false,
                        |index, bytes| {
                            let at = small[index].0;
                            let (name, _) = &chunk[at - start];
                            entries[at] = entry(name.clone(), bytes.len() as u64, bytes);
                            Ok(())
                        },
                    )?;
                }
            }
        } else {
            let snapshot = self.snapshot(rev, Default::default()).await?;
            let mut pending = vec![GitPath::root()];
            while let Some(directory) = pending.pop() {
                let mut cursor = None;
                loop {
                    let page = snapshot
                        .tree(directory.clone(), PageRequest::new(1000, cursor)?)
                        .await?;
                    for item in page.items {
                        let name = String::from_utf8(item.path.as_bytes().to_vec())?;
                        if item.mode == EntryMode::Tree {
                            pending.push(item.path.clone());
                        }
                        if prefix.is_some_and(|prefix| !name.starts_with(prefix)) {
                            continue;
                        }
                        let kind = match item.mode {
                            EntryMode::Tree => EntryKind::Dir,
                            EntryMode::Submodule => EntryKind::Submodule,
                            _ => EntryKind::File,
                        };
                        if matches!(kind, EntryKind::Dir | EntryKind::Submodule) {
                            entries.push(BrowserEntry {
                                path: name.into(),
                                kind,
                                size: None,
                                pointer_hash: None,
                            });
                        } else {
                            let metadata = snapshot.file_metadata(item.path.clone()).await?;
                            if metadata.kind == ContentKind::Git {
                                entries.push(BrowserEntry {
                                    path: name.into(),
                                    kind: EntryKind::File,
                                    size: Some(metadata.size),
                                    pointer_hash: None,
                                });
                            } else {
                                let blob = snapshot.read_blob(item.path).await?;
                                entries.push(entry(name, metadata.git_size, &blob));
                            }
                        }
                    }
                    cursor = page.next;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
        }
        Ok(entries)
    }

    pub async fn stat(&self, path: &str, rev: Option<&str>) -> Result<FileMetadata> {
        GitPath::new(path.as_bytes().to_vec())?;
        if self.remote.is_some() {
            let snapshot = self.snapshot(rev, Default::default()).await?;
            return remote_metadata(&snapshot, path, rev).await;
        }
        let entry = self
            .list(Some(path), rev)
            .await?
            .into_iter()
            .find(|entry| entry.path == Path::new(path))
            .ok_or_else(|| AgentError::PathNotFound {
                path: path.to_owned(),
            })?;
        Ok(FileMetadata {
            size: entry.size.unwrap_or(0),
            pointer_hash: entry.pointer_hash,
            mtime: self
                .work_dir
                .as_ref()
                .and_then(|root| std::fs::metadata(root.join(path)).ok())
                .and_then(|m| m.modified().ok()),
            rev: rev.map(str::to_owned),
            kind: match entry.kind {
                EntryKind::Pointer => ContentKind::Crab,
                EntryKind::Lfs => ContentKind::Lfs,
                _ => ContentKind::Git,
            },
        })
    }
}

async fn remote_metadata(
    snapshot: &Snapshot,
    path: &str,
    rev: Option<&str>,
) -> Result<FileMetadata> {
    let path = GitPath::new(path.as_bytes().to_vec())?;
    let metadata = snapshot.file_metadata(path.clone()).await?;
    let pointer_hash = if metadata.kind == ContentKind::Git {
        None
    } else {
        let pointer = snapshot.read_blob(path).await?;
        entry(String::new(), metadata.git_size, &pointer).pointer_hash
    };
    Ok(FileMetadata {
        size: metadata.size,
        pointer_hash,
        mtime: None,
        rev: rev.map(str::to_owned),
        kind: metadata.kind,
    })
}

fn revision(value: &str) -> Result<Revision> {
    if let Some(tag) = value.strip_prefix("refs/tags/") {
        return Ok(Revision::tag(tag)?);
    }
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Revision::commit(crab_sdk::ObjectId::from_hex(value)?));
    }
    Ok(Revision::branch(
        value.strip_prefix("refs/heads/").unwrap_or(value),
    )?)
}

fn entry(path: String, size: u64, blob: &[u8]) -> BrowserEntry {
    let (kind, size, hash) = match crab_git::classify(blob) {
        crab_git::PointerKind::Crab(pointer) => (
            EntryKind::Pointer,
            pointer.size,
            Some(crab_git::lfs_pointer::hex_encode(&pointer.file_hash)),
        ),
        crab_git::PointerKind::Lfs(pointer) => (
            EntryKind::Lfs,
            pointer.size,
            Some(crab_git::lfs_pointer::hex_encode(&pointer.oid)),
        ),
        crab_git::PointerKind::NotAPointer => (EntryKind::File, size, None),
    };
    BrowserEntry {
        path: path.into(),
        kind,
        size: Some(size),
        pointer_hash: hash,
    }
}

async fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = crate::shellout::git_command()?
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

async fn git_batch(root: &Path, mode: &str, oids: &[&str]) -> Result<Vec<u8>> {
    use std::process::Stdio;
    let mut child = crate::shellout::git_command()?
        .args(["cat-file", mode])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Other("Git stdin unavailable".into()))?;
    let input = format!("{}\n", oids.join("\n"));
    let write = async move {
        let result = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
        result
    };
    let (written, output) = tokio::join!(write, child.wait_with_output());
    written?;
    let output = output?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn text(session: &BrowseSession, path: &str, rev: Option<&str>) -> Result<String> {
        let source = FileSource::capture(session, path, rev, session.read_scope()).await?;
        let bytes = source.read_complete(1024).await;
        let closed = source.close().await;
        let bytes = bytes?;
        closed?;
        Ok(String::from_utf8(bytes.to_vec())?)
    }

    #[tokio::test]
    #[ignore = "requires an isolated CRAB_SDK_TEST_S3_BUCKET fixture"]
    async fn native_v2_snapshot_reads_committed_content() {
        use crab_sdk::remote::write::{CommitIdentity, CommitOptions, FileEdit};
        let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::s3_from_env(&bucket).unwrap())
            .build()
            .unwrap();
        let result: Result<String> = async {
            let prefix = format!("desktop-test/{}", uuid::Uuid::new_v4());
            let locator = RepositoryLocator::new(&prefix).unwrap();
            client
                .initialize_remote(locator.clone(), "refs/heads/main")
                .await?;
            let repository = client.open(OpenOptions::remote(locator.clone())).await?;
            let identity =
                CommitIdentity::new("Desktop test", "test@example.invalid", 1_700_000_000, 0)?;
            let bytes = b"native v2 content\n".to_vec();
            let _outcome = repository
                .remote()?
                .prepare_commit(
                    CommitOptions::initial(
                        "refs/heads/main",
                        identity.clone(),
                        identity,
                        b"initial\n".to_vec(),
                    )?,
                    vec![FileEdit::git(
                        GitPath::new("file.txt")?,
                        EntryMode::Regular,
                        bytes.len() as u64,
                        std::io::Cursor::new(bytes),
                    )?],
                    scratch.path().to_path_buf(),
                )
                .await?
                .execute()
                .await?;
            let repository = client.open(OpenOptions::remote(locator)).await?;
            let session = BrowseSession {
                url: "test".into(),
                work_dir: None,
                readers: native::NativeReads::default(),
                files: files::Files::default(),
                remote: Some(Arc::new(RemoteSession {
                    client: client.clone(),
                    repository,
                })),
            };
            text(&session, "file.txt", None).await
        }
        .await;
        client.close().await.unwrap();
        assert_eq!(result.unwrap(), "native v2 content\n");
    }

    #[tokio::test]
    async fn local_browse_distinguishes_worktree_bytes_from_committed_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            git(root, &args).await.unwrap();
        }
        tokio::fs::write(root.join("file.txt"), b"committed\n")
            .await
            .unwrap();
        git(root, &["add", "file.txt"]).await.unwrap();
        git(
            root,
            &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
        )
        .await
        .unwrap();
        tokio::fs::write(root.join("file.txt"), b"edited\n")
            .await
            .unwrap();
        let session = BrowseSession::open_local(root, &crate::repository::ReadScope::default())
            .await
            .unwrap();
        let result = async {
            assert_eq!(text(&session, "file.txt", None).await.unwrap(), "edited\n");
            assert_eq!(
                text(&session, "file.txt", Some("HEAD")).await.unwrap(),
                "committed\n"
            );
            assert!(matches!(
                session.stat("missing", None).await,
                Err(AgentError::PathNotFound { .. })
            ));
        }
        .await;
        session.close().await.unwrap();
        result
    }
}
