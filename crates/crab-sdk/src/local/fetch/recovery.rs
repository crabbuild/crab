use std::path::{Path, PathBuf};

use super::{FETCH_INTENT, FetchIntent, MetadataEdit};
use crate::{Error, ErrorKind, Result};

pub(super) async fn read_optional_utf8(path: &Path, label: &'static str) -> Result<Option<String>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::with_source(ErrorKind::Io, label, source)),
    };
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "local Git metadata exceeds 64 MiB",
        ));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|source| Error::with_source(ErrorKind::Corruption, label, source))
}

pub(super) async fn write_fetch_intent(git_dir: &Path, intent: &FetchIntent) -> Result<()> {
    let bytes = serde_json::to_vec(intent).map_err(|source| {
        Error::with_source(ErrorKind::Io, "cannot encode SDK fetch intent", source)
    })?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "SDK fetch intent exceeds 64 MiB",
        ));
    }
    let temporary = git_dir.join(format!(".{FETCH_INTENT}.tmp-{}", uuid::Uuid::new_v4()));
    let target = git_dir.join(FETCH_INTENT);
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot create SDK fetch intent", source)
        })?;
    tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot write SDK fetch intent", source)
        })?;
    file.sync_all().await.map_err(|source| {
        Error::with_source(ErrorKind::Io, "cannot sync SDK fetch intent", source)
    })?;
    drop(file);
    tokio::fs::rename(&temporary, &target)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot publish SDK fetch intent", source)
        })?;
    sync_parent(&target, "cannot sync SDK fetch intent directory").await
}

pub(super) async fn read_fetch_intent(git_dir: &Path) -> Result<FetchIntent> {
    let path = git_dir.join(FETCH_INTENT);
    let bytes = tokio::fs::read(&path).await.map_err(|source| {
        Error::with_source(ErrorKind::Io, "cannot read SDK fetch intent", source)
    })?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(Error::new(
            ErrorKind::Corruption,
            "SDK fetch intent exceeds 64 MiB",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|source| {
        Error::with_source(ErrorKind::Corruption, "SDK fetch intent is invalid", source)
    })
}

pub(super) async fn remove_fetch_intent(git_dir: &Path) -> Result<()> {
    let path = git_dir.join(FETCH_INTENT);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => sync_parent(&path, "cannot sync SDK fetch recovery").await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::with_source(
            ErrorKind::Io,
            "cannot clear SDK fetch intent",
            source,
        )),
    }
}

pub(super) async fn apply_file_edit(
    path: &Path,
    edit: &MetadataEdit,
    label: &'static str,
) -> Result<()> {
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = PathBuf::from(lock_name);
    let mut file = match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return recover_file_edit_lock(path, &lock, edit, label).await;
        }
        Err(source) => return Err(Error::with_source(ErrorKind::Conflict, label, source)),
    };
    let applied = async {
        let current = read_optional_utf8(path, label).await?;
        if current == edit.after {
            return Ok(false);
        }
        if current != edit.before {
            return Err(Error::new(
                ErrorKind::Conflict,
                "local metadata changed while recovering fetch",
            ));
        }
        if let Some(after) = &edit.after {
            tokio::io::AsyncWriteExt::write_all(&mut file, after.as_bytes())
                .await
                .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
            file.sync_all()
                .await
                .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
            drop(file);
            tokio::fs::rename(&lock, path)
                .await
                .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
            Ok(true)
        } else {
            tokio::io::AsyncWriteExt::write_all(&mut file, b"crab-sdk-fetch-delete-v1\n")
                .await
                .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
            file.sync_all()
                .await
                .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
            drop(file);
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(Error::with_source(ErrorKind::Io, label, source)),
            }
            Ok(false)
        }
    }
    .await;
    match applied {
        Ok(true) => sync_parent(path, label).await,
        Ok(false) => {
            remove_owned_lock(&lock, label).await?;
            sync_parent(path, label).await
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&lock).await;
            Err(error)
        }
    }
}

async fn recover_file_edit_lock(
    path: &Path,
    lock: &Path,
    edit: &MetadataEdit,
    label: &'static str,
) -> Result<()> {
    let current = read_optional_utf8(path, label).await?;
    let staged = read_optional_utf8(lock, label).await?;
    let expected_lock = edit
        .after
        .as_deref()
        .unwrap_or("crab-sdk-fetch-delete-v1\n");
    if staged.as_deref() != Some(expected_lock) {
        return Err(Error::new(
            ErrorKind::Conflict,
            "local Git metadata is locked by another process",
        ));
    }
    if current == edit.after {
        remove_owned_lock(lock, label).await?;
        return sync_parent(path, label).await;
    }
    if current != edit.before {
        return Err(Error::new(
            ErrorKind::Conflict,
            "local metadata changed while recovering fetch",
        ));
    }
    if edit.after.is_some() {
        tokio::fs::rename(lock, path)
            .await
            .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
    } else {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::with_source(ErrorKind::Io, label, source)),
        }
        remove_owned_lock(lock, label).await?;
    }
    sync_parent(path, label).await
}

async fn remove_owned_lock(path: &Path, label: &'static str) -> Result<()> {
    tokio::fs::remove_file(path)
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, label, source))
}

#[cfg(unix)]
async fn sync_parent(path: &Path, label: &'static str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(ErrorKind::Io, label))?;
    let parent = parent.to_owned();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?
        .map_err(|source| Error::with_source(ErrorKind::Io, label, source))
}

#[cfg(not(unix))]
async fn sync_parent(_path: &Path, _label: &'static str) -> Result<()> {
    Ok(())
}
