//! Direct capsule-bound transfer into a local Git object database.

use std::collections::BTreeMap;
use std::path::Path;

use crab_read::capsule_protocol::{
    CapsuleReadLimits, install_git_packs_from_store, open_git_view_from_root,
};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

/// Exact remote state whose immutable packs were installed locally.
#[derive(Clone, Debug)]
pub struct FetchSnapshot {
    pub generation: u64,
    pub refs: BTreeMap<String, String>,
    pub peeled_refs: BTreeMap<String, String>,
    pub head: String,
}

/// Failure while transferring a pinned Crab capsule view into a local ODB.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("direct transfer cancelled")]
    Cancelled,
    #[error("cannot read the remote capsule authority")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("cannot validate or install the captured capsule packs")]
    Read(#[from] crab_read::ReadError),
}

/// Install all Git packs authenticated by one captured capsule view.
///
/// The caller's store retains read admission. Refs are returned only after
/// authenticated packs and external delta bases validate and land. Cancellation
/// drains pack installers before returning; no legacy repository keys are read.
pub async fn fetch_snapshot(
    layout: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    cancel: &CancellationToken,
) -> Result<FetchSnapshot, Error> {
    let capture = async {
        let root = crab_metadata::capsule_protocol::load_root(layout).await?;
        Ok::<_, Error>(
            open_git_view_from_root(
                layout,
                root,
                CapsuleReadLimits {
                    max_capsule_bytes: max_input_bytes,
                    max_frontier_bytes: max_input_bytes,
                },
            )
            .await?,
        )
    };
    let view = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(Error::Cancelled),
        result = capture => result?,
    };
    install_git_packs_from_store(&view, layout, git_dir, max_input_bytes, None, cancel).await?;
    Ok(FetchSnapshot {
        generation: view.root().root().generation(),
        refs: view.refs().clone(),
        peeled_refs: view.peeled_refs().clone(),
        head: view.head().to_owned(),
    })
}
