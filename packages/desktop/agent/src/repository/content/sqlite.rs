use super::*;

impl FileSource {
    pub(crate) async fn capture_sqlite(
        repo: &BrowseSession,
        path: &str,
        rev: Option<&str>,
        scope: ReadScope,
    ) -> Result<(Self, Option<Self>)> {
        scope.check()?;
        let validated = GitPath::new(path.as_bytes().to_vec())?;
        let journal = format!("{path}-wal");
        match (&repo.work_dir, rev) {
            (Some(root), None) => {
                let database =
                    Self::working_file(repo, root, Path::new(path), scope.clone()).await?;
                let journal = match database.source.as_ref() {
                    Source::Local { path, .. } => {
                        let mut physical = path.as_os_str().to_owned();
                        physical.push("-wal");
                        PathBuf::from(physical)
                    }
                    _ => PathBuf::from(journal),
                };
                let journal = match Self::working_file(repo, root, &journal, scope).await {
                    Ok(journal) => Some(journal),
                    Err(AgentError::PathNotFound { .. }) => None,
                    Err(error) => return Err(error),
                };
                Ok((database, journal))
            }
            (Some(root), Some(rev)) => {
                let index = rev.is_empty();
                let entries = if index {
                    command::run(
                        root,
                        &["ls-files", "--stage", "-z", "--", path, &journal],
                        &scope,
                    )
                    .await?
                } else {
                    let tree = format!("{rev}^{{tree}}");
                    let tree = command::run(
                        root,
                        &["rev-parse", "--verify", "--end-of-options", &tree],
                        &scope,
                    )
                    .await?;
                    let tree = std::str::from_utf8(&tree)
                        .map_err(std::io::Error::other)?
                        .trim();
                    command::run(root, &["ls-tree", "-z", tree, "--", path, &journal], &scope)
                        .await?
                };
                let database_oid = git_file(&entries, path, index)?
                    .ok_or_else(|| AgentError::PathNotFound { path: path.into() })?;
                let journal_oid = git_file(&entries, &journal, index)?;
                let database = Self::git_blob(repo, root, database_oid, scope.clone()).await?;
                let journal = match journal_oid {
                    Some(oid) => Some(Self::git_blob(repo, root, oid, scope).await?),
                    None => None,
                };
                Ok((database, journal))
            }
            (None, _) => {
                let snapshot = scope.run(repo.snapshot(rev, scope.options())).await?;
                let journal_path = GitPath::new(journal.as_bytes().to_vec())?;
                let has_journal =
                    remote_journal_exists(&snapshot, path, &journal_path, &scope).await?;
                let database =
                    Self::snapshot_file(snapshot.clone(), validated, scope.clone()).await?;
                let journal = if has_journal {
                    Some(Self::snapshot_file(snapshot, journal_path, scope).await?)
                } else {
                    None
                };
                Ok((database, journal))
            }
        }
    }

    pub(crate) async fn live_sqlite_path(&self) -> Result<Option<&Path>> {
        let Source::Local { path, modified, .. } = self.source.as_ref() else {
            return Ok(None);
        };
        self.scope.check()?;
        let mut active = false;
        for suffix in ["-wal", "-journal", "-shm"] {
            let mut companion = path.as_os_str().to_owned();
            companion.push(suffix);
            match tokio::fs::symlink_metadata(companion).await {
                Ok(metadata) if metadata.is_file() => {
                    active |= suffix != "-shm" && metadata.len() != 0;
                }
                Ok(_) => {
                    return Err(AgentError::Other(
                        "SQLite companion must be a regular file".into(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !active {
            return Ok(None);
        }
        let metadata = tokio::fs::symlink_metadata(path).await?;
        if !metadata.is_file()
            || metadata.len() != self.size
            || metadata.modified().ok() != *modified
        {
            return Err(changed());
        }
        self.scope.check()?;
        Ok(Some(path))
    }

    pub(crate) fn is_plain_working_file(&self) -> bool {
        matches!(self.source.as_ref(), Source::Local { .. })
    }
}

fn git_file(entries: &[u8], path: &str, index: bool) -> Result<Option<String>> {
    let (mode, oid) = command::file_entry_at(entries, path, index)?;
    if mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "100644" | "100755"))
    {
        return Err(AgentError::Other(
            "SQLite database and WAL must be regular files".into(),
        ));
    }
    Ok(oid)
}

async fn remote_journal_exists(
    snapshot: &Snapshot,
    path: &str,
    journal: &GitPath,
    scope: &ReadScope,
) -> Result<bool> {
    let parent = path.rsplit_once('/').map(|(parent, _)| parent);
    let directory = match parent {
        Some(parent) => GitPath::new(parent.as_bytes().to_vec())?,
        None => GitPath::root(),
    };
    let mut cursor = None;
    loop {
        scope.check()?;
        let request = PageRequest::new(1000, cursor)?;
        // Metadata NotFound also covers missing objects. Only the immutable
        // parent tree can prove an optional WAL entry is absent.
        let page = scope
            .run(async {
                Ok(snapshot
                    .tree(directory.clone(), request)
                    .with_options(ReadOptions::default().with_operation(scope.options()))
                    .await?)
            })
            .await?;
        if let Some(entry) = page.items.iter().find(|entry| entry.path == *journal) {
            if !matches!(entry.mode, EntryMode::Regular | EntryMode::Executable) {
                return Err(AgentError::Other(
                    "SQLite WAL must be a regular file".into(),
                ));
            }
            return Ok(true);
        }
        cursor = page.next;
        if cursor.is_none() {
            return Ok(false);
        }
    }
}
