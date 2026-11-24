use std::sync::Arc;

use crab_remote_git::{
    ContentClassification, EntryKind, OperationContext, RemoteGitSnapshot, TreeEntry,
};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    repository::Repository,
    server::Server,
};

const DEFAULTS_REPOSITORY: &str = ".github";
const TEMPLATE_BYTES: u64 = 32 * 1024;

pub(super) fn source(server: &Server, repo: &Repository) -> Option<Arc<Repository>> {
    if repo.config.name == DEFAULTS_REPOSITORY
        || (repo.organization.is_none()
            && !crate::catalog::is_personal_namespace(&repo.config.owner))
    {
        return None;
    }
    server
        .repositories
        .get(&(repo.config.owner.clone(), DEFAULTS_REPOSITORY.into()))
        .filter(|source| source.id != repo.id && source.organization == repo.organization)
}

pub(super) async fn readable(
    server: &Server,
    principal: &Principal,
    destination: &Repository,
    source: &Repository,
) -> Result<bool> {
    let Some(reader) = principal.related_read(destination.id, source.id) else {
        return Ok(false);
    };
    match app::membership(server, &reader, source).await {
        Ok(_) => Ok(true),
        Err(Error::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) async fn template_text(
    snapshot: &RemoteGitSnapshot,
    entry: &TreeEntry,
    operation: &OperationContext,
) -> crab_remote_git::Result<Option<String>> {
    if entry.kind != EntryKind::Blob
        || operation.read_object_metadata(entry.oid).await?.size > TEMPLATE_BYTES
    {
        return Ok(None);
    }
    let blob = snapshot.read_blob(&entry.path, operation).await?;
    if blob.metadata.classification != ContentClassification::OrdinaryGit || blob.bytes.contains(&0)
    {
        return Ok(None);
    }
    Ok(String::from_utf8(blob.bytes.into()).ok())
}
