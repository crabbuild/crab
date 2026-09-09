//! Direct generation-bound transfer into a local Git object database.

use std::collections::BTreeMap;
use std::path::Path;

use crab_metadata::manifest_store::RepositorySnapshot;
use crab_metadata::manifests::PackManifestEntry;
use crab_storage::{Store, StoreLayout};
use futures_util::{TryStreamExt, stream};
use tokio_util::sync::CancellationToken;

const DOWNLOAD_CONCURRENCY: usize = 8;

/// Exact remote state whose immutable packs were installed locally.
#[derive(Clone, Debug)]
pub struct FetchSnapshot {
    pub generation: u64,
    pub refs: BTreeMap<String, String>,
    pub peeled_refs: BTreeMap<String, String>,
    pub head: String,
}

/// Failure while transferring a pinned Crab generation into a local ODB.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("direct transfer cancelled")]
    Cancelled,
    #[error("cannot read the remote repository snapshot")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("cannot download a remote Git artifact")]
    Storage(#[from] crab_storage::StorageError),
    #[error("cannot validate or install a remote Git pack")]
    Pack(#[from] crab_git::PackError),
    #[error("cannot create direct-transfer scratch space")]
    Scratch(#[source] std::io::Error),
    #[error("direct pack installer stopped")]
    Installer(#[source] tokio::task::JoinError),
}

/// Install every immutable pack visible from one coherent remote snapshot.
///
/// Downloads are bounded by committed sizes and pack-derived sidecar limits.
/// Refs are returned only after every pack and sidecar validates and lands.
pub async fn fetch_snapshot(
    store: &Store,
    layout: &StoreLayout<Store>,
    pack_directory: &Path,
    cancel: &CancellationToken,
) -> Result<FetchSnapshot, Error> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    tokio::fs::create_dir_all(pack_directory)
        .await
        .map_err(Error::Scratch)?;
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
    stream::iter(snapshot.journal.packs.iter().cloned().map(Ok::<_, Error>))
        .map_ok(|pack| install_pack(store, layout, pack_directory, pack, cancel))
        .try_buffer_unordered(DOWNLOAD_CONCURRENCY)
        .try_for_each(|()| async { Ok(()) })
        .await?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(fetch_state(snapshot))
}

fn fetch_state(snapshot: RepositorySnapshot) -> FetchSnapshot {
    FetchSnapshot {
        generation: snapshot.manifest.generation,
        refs: snapshot.journal.refs,
        peeled_refs: snapshot.journal.peeled_refs,
        head: snapshot.journal.head,
    }
}

async fn install_pack(
    store: &Store,
    layout: &StoreLayout<Store>,
    pack_directory: &Path,
    pack: PackManifestEntry,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    if installed(pack_directory, &pack) {
        return Ok(());
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let scratch = tempfile::Builder::new()
        .prefix(".crab-direct-fetch-")
        .tempdir_in(pack_directory)
        .map_err(Error::Scratch)?;
    let pack_path = scratch.path().join("pack");
    let index_path = scratch.path().join("index");
    let reverse_path = scratch.path().join("reverse");
    let index_limit = crab_git::max_pack_index_size(pack.object_count).ok_or_else(|| {
        Error::Pack(crab_git::PackError::SidecarSizeOverflow {
            path: index_path.clone(),
        })
    })?;
    let reverse_limit = crab_git::pack_reverse_index_size(pack.object_count).ok_or_else(|| {
        Error::Pack(crab_git::PackError::SidecarSizeOverflow {
            path: reverse_path.clone(),
        })
    })?;
    let remote_pack_path = layout.pack_path(&pack.pack_id);
    let remote_index_path = layout.pack_index_path(&pack.pack_id);
    let remote_reverse_path = layout.pack_reverse_index_path(&pack.pack_id);
    let downloads = async {
        let (pack_size, _, _) = tokio::try_join!(
            store.download_to_path_bounded(&remote_pack_path, &pack_path, pack.size,),
            store.download_to_path_bounded(&remote_index_path, &index_path, index_limit,),
            store.download_to_path_bounded(&remote_reverse_path, &reverse_path, reverse_limit,),
        )?;
        if pack_size != pack.size {
            return Err(crab_storage::StorageError::CorruptObject {
                path: layout.pack_path(&pack.pack_id).to_string(),
                reason: format!(
                    "downloaded pack size {pack_size} differs from committed size {}",
                    pack.size
                ),
            });
        }
        Ok::<_, crab_storage::StorageError>(())
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        result = downloads => result?,
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let destination = pack_directory.to_owned();
    let canonical = pack.pack_id.clone();
    let size = pack.size;
    let objects = pack.object_count;
    tokio::task::spawn_blocking(move || {
        crab_git::pack::install_pack_files_from_paths(
            &destination,
            &pack_path,
            &index_path,
            &reverse_path,
            &canonical,
            size,
            objects,
        )
    })
    .await
    .map_err(Error::Installer)??;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn installed(pack_directory: &Path, pack: &PackManifestEntry) -> bool {
    let artifact = |extension| pack_directory.join(format!("pack-{}.{extension}", pack.pack_id));
    let pack_path = artifact("pack");
    let index_path = artifact("idx");
    let reverse_path = artifact("rev");
    let Ok(metadata) = std::fs::metadata(&pack_path) else {
        return false;
    };
    if metadata.len() != pack.size {
        return false;
    }
    crab_git::PackLocationIter::open(&index_path, &reverse_path, pack.size)
        .is_ok_and(|locations| locations.object_count() == pack.object_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_pack_is_not_installed() {
        let directory = tempfile::tempdir().unwrap();
        let pack = PackManifestEntry {
            pack_id: "a".repeat(64),
            size: 12,
            content_hash: "a".repeat(64),
            ref_tips: vec![],
            object_count: 1,
        };

        assert!(!installed(directory.path(), &pack));
    }
}
