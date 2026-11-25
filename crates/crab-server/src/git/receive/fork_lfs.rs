use std::{collections::BTreeMap, io};

use crab_git::{pointer_detect::PointerKind, receive_plan::PointerDependency};
use crab_lfs::{LfsError, LfsObjectStore};
use crab_read::dependency_proof::DependencyProofError;
use futures_util::{StreamExt, TryStreamExt, stream};
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result};
use crate::{app, auth::Principal, repository::Repository, server::Server};

pub(super) async fn copy(
    server: &Server,
    principal: &Principal,
    destination: &Repository,
    source: &Repository,
    pointers: &[PointerDependency],
    max_dependencies: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    if pointers.len() > max_dependencies {
        return Err(ReceiveError::Dependency(Box::new(
            DependencyProofError::Limit("pointer count"),
        )));
    }
    let mut objects = BTreeMap::new();
    for dependency in pointers {
        let PointerKind::Lfs(pointer) = &dependency.pointer else {
            continue;
        };
        if let Some((_, size)) = objects.insert(pointer.oid, (dependency.blob, pointer.size))
            && size != pointer.size
        {
            return Err(ReceiveError::Dependency(Box::new(
                DependencyProofError::Invalid {
                    blob: dependency.blob,
                    reason: "conflicting sizes for the same LFS object",
                },
            )));
        }
    }
    if objects.is_empty() {
        return Ok(());
    }
    let upstream = LfsObjectStore::new(source.store.clone(), &source.config.prefix);
    let fork = LfsObjectStore::new(destination.store.clone(), &destination.config.prefix);
    for (oid, (blob, size)) in objects {
        if cancel.is_cancelled() {
            return Err(ReceiveError::Cancelled);
        }
        if !app::membership(server, principal, destination)
            .await?
            .principal
            .can_write(destination.id)
        {
            return Err(ReceiveError::Forbidden);
        }
        if destination.forked_from == Some(source.id) && principal.import_grant().is_none() {
            app::require_related_read(server, principal, destination, source).await?;
        }
        // Use the same verified stream and SHA-256 publisher as native
        // transfers. An already verified destination skips the source read.
        let download = stream::once(async {
            upstream
                .get_stream(&oid, size, None)
                .await
                .map(|(_, _, bytes)| bytes)
        })
        .try_flatten();
        let download = stream::unfold(Box::pin(download), |mut download| async move {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => Some(Err(LfsError::from(io::Error::from(io::ErrorKind::Interrupted)))),
                next = download.next() => next,
            };
            next.map(|item| (item, download))
        });
        // Drive cancellation through the input stream so the multipart
        // publisher can abort and drain before the ref lease is released.
        fork.put_byte_stream_with_size(&oid, size, download)
            .await
            .map_err(|source| {
                if cancel.is_cancelled() {
                    ReceiveError::Cancelled
                } else {
                    ReceiveError::Dependency(Box::new(DependencyProofError::Lfs { blob, source }))
                }
            })?;
    }
    Ok(())
}
