use std::collections::{BTreeMap, HashMap, VecDeque};

use futures_util::future::BoxFuture;
use gix_hash::ObjectId;
use gix_object::{Kind, bstr::BString, tree};

use crate::{
    auth::Identity,
    git_objects::{self, commit_bytes, encode_tree, object_id, read_tree},
};

const MAX_ANCESTRY_COMMITS: usize = 100_000;
const MAX_MERGE_BLOB_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct Plan {
    pub oid: ObjectId,
    pub objects: Vec<(Kind, Vec<u8>)>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("the pull request branches do not have the recorded base in their history")]
    History,
    #[error("the pull request changes conflict")]
    Conflict,
    #[error(transparent)]
    Object(#[from] git_objects::Error),
}

pub(super) async fn build(
    repository: &crab_remote_git::RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    current: ObjectId,
    other: ObjectId,
    actor: &Identity,
    message: &str,
    seconds: u64,
) -> Result<Plan, Error> {
    let ancestor = merge_base(operation, current, other).await?;
    let ancestor_tree = repository
        .snapshot(&crab_remote_git::Revision::Commit(ancestor), operation)
        .await
        .map_err(git_objects::Error::from)?
        .root_tree_oid();
    let current_tree = repository
        .snapshot(&crab_remote_git::Revision::Commit(current), operation)
        .await
        .map_err(git_objects::Error::from)?
        .root_tree_oid();
    let other_tree = repository
        .snapshot(&crab_remote_git::Revision::Commit(other), operation)
        .await
        .map_err(git_objects::Error::from)?
        .root_tree_oid();
    let mut objects = Vec::new();
    let tree = merge_tree(
        operation,
        Some(ancestor_tree),
        current_tree,
        other_tree,
        &mut objects,
    )
    .await?;
    let commit = commit_bytes(tree, &[current, other], actor, message, seconds);
    let oid = object_id(Kind::Commit, &commit).map_err(git_objects::Error::from)?;
    objects.push((Kind::Commit, commit));
    Ok(Plan { oid, objects })
}

async fn merge_base(
    operation: &crab_remote_git::OperationContext,
    current: ObjectId,
    other: ObjectId,
) -> Result<ObjectId, Error> {
    let mut current_pending = VecDeque::from([(current, 0usize)]);
    let mut other_pending = VecDeque::from([(other, 0usize)]);
    let mut current_depths = HashMap::from([(current, 0usize)]);
    let mut other_depths = HashMap::from([(other, 0usize)]);
    let mut best: Option<(usize, ObjectId)> = None;
    loop {
        let current_depth = current_pending.front().map(|(_, depth)| *depth);
        let other_depth = other_pending.front().map(|(_, depth)| *depth);
        if best.is_some_and(|(distance, _)| {
            current_depth.unwrap_or(0) + other_depth.unwrap_or(0) >= distance
        }) {
            break;
        }
        let expand_current = match (current_depth, other_depth) {
            (Some(current), Some(other)) => current <= other,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if current_depths.len() + other_depths.len() > MAX_ANCESTRY_COMMITS {
            return Err(Error::History);
        }
        let (pending, own, opposite) = if expand_current {
            (&mut current_pending, &mut current_depths, &other_depths)
        } else {
            (&mut other_pending, &mut other_depths, &current_depths)
        };
        let Some((oid, depth)) = pending.pop_front() else {
            continue;
        };
        if let Some(opposite_depth) = opposite.get(&oid) {
            let distance = depth + opposite_depth;
            if best.is_none_or(|(best_distance, _)| distance < best_distance) {
                best = Some((distance, oid));
            }
        }
        for parent in read_parents(operation, oid).await? {
            if let std::collections::hash_map::Entry::Vacant(entry) = own.entry(parent) {
                entry.insert(depth + 1);
                pending.push_back((parent, depth + 1));
            }
        }
    }
    best.map(|(_, oid)| oid).ok_or(Error::History)
}

async fn read_parents(
    operation: &crab_remote_git::OperationContext,
    oid: ObjectId,
) -> Result<Vec<ObjectId>, Error> {
    let object = operation
        .read_object(oid)
        .await
        .map_err(git_objects::Error::from)?;
    if object.kind != Kind::Commit {
        return Err(Error::History);
    }
    let commit = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(git_objects::Error::from)?;
    Ok(commit.parents().collect())
}

fn merge_tree<'a>(
    operation: &'a crab_remote_git::OperationContext,
    ancestor_oid: Option<ObjectId>,
    current_oid: ObjectId,
    other_oid: ObjectId,
    objects: &'a mut Vec<(Kind, Vec<u8>)>,
) -> BoxFuture<'a, Result<ObjectId, Error>> {
    Box::pin(async move {
        if current_oid == other_oid {
            return Ok(current_oid);
        }
        if ancestor_oid == Some(current_oid) {
            return Ok(other_oid);
        }
        if ancestor_oid == Some(other_oid) {
            return Ok(current_oid);
        }
        let ancestor = match ancestor_oid {
            Some(oid) => read_tree(operation, oid).await?,
            None => vec![],
        };
        let current = read_tree(operation, current_oid).await?;
        let other = read_tree(operation, other_oid).await?;
        let ancestor = entries_by_name(ancestor);
        let current = entries_by_name(current);
        let other = entries_by_name(other);
        let names = ancestor
            .keys()
            .chain(current.keys())
            .chain(other.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut merged = Vec::with_capacity(names.len());
        for name in names {
            let entry = merge_entry(
                operation,
                ancestor.get(&name),
                current.get(&name),
                other.get(&name),
                objects,
            )
            .await?;
            if let Some(mut entry) = entry {
                entry.filename = BString::from(name);
                merged.push(entry);
            }
        }
        encode_tree(merged, objects).map_err(Error::from)
    })
}

fn merge_entry<'a>(
    operation: &'a crab_remote_git::OperationContext,
    ancestor: Option<&'a tree::Entry>,
    current: Option<&'a tree::Entry>,
    other: Option<&'a tree::Entry>,
    objects: &'a mut Vec<(Kind, Vec<u8>)>,
) -> BoxFuture<'a, Result<Option<tree::Entry>, Error>> {
    Box::pin(async move {
        if same_entry(current, other) {
            return Ok(current.cloned());
        }
        if same_entry(current, ancestor) {
            return Ok(other.cloned());
        }
        if same_entry(other, ancestor) {
            return Ok(current.cloned());
        }
        match (ancestor, current, other) {
            (ancestor, Some(current), Some(other))
                if current.mode.is_tree()
                    && other.mode.is_tree()
                    && ancestor.is_none_or(|entry| entry.mode.is_tree()) =>
            {
                let oid = merge_tree(
                    operation,
                    ancestor.map(|entry| entry.oid),
                    current.oid,
                    other.oid,
                    objects,
                )
                .await?;
                Ok(Some(tree::Entry {
                    mode: current.mode,
                    filename: current.filename.clone(),
                    oid,
                }))
            }
            (Some(ancestor), Some(current), Some(other))
                if !ancestor.mode.is_tree() && !current.mode.is_tree() && !other.mode.is_tree() =>
            {
                merge_blob(operation, ancestor, current, other, objects)
                    .await
                    .map(Some)
            }
            (None, Some(_), None) => Ok(current.cloned()),
            (None, None, Some(_)) => Ok(other.cloned()),
            _ => Err(Error::Conflict),
        }
    })
}

async fn merge_blob(
    operation: &crab_remote_git::OperationContext,
    ancestor: &tree::Entry,
    current: &tree::Entry,
    other: &tree::Entry,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<tree::Entry, Error> {
    let mode = if current.mode == other.mode {
        current.mode
    } else if current.mode == ancestor.mode {
        other.mode
    } else if other.mode == ancestor.mode {
        current.mode
    } else {
        return Err(Error::Conflict);
    };
    if mode.kind() == tree::EntryKind::Commit || mode.kind() == tree::EntryKind::Link {
        return Err(Error::Conflict);
    }
    let filename = current.filename.clone();
    let ancestor_bytes = read_blob(operation, ancestor.oid).await?;
    let current_bytes = read_blob(operation, current.oid).await?;
    let other_bytes = read_blob(operation, other.oid).await?;
    let mut input = gix_diff::blob::InternedInput::new(&[][..], &[][..]);
    let mut merged = Vec::new();
    let options = gix_merge::blob::builtin_driver::text::Options::default();
    let merge = gix_merge::blob::builtin_driver::text::Merge::new(
        &mut input,
        &current_bytes,
        &ancestor_bytes,
        &other_bytes,
        options.diff_algorithm,
    );
    let resolution = merge.run(&mut merged, Default::default(), options.conflict);
    if resolution == gix_merge::blob::Resolution::Conflict {
        return Err(Error::Conflict);
    }
    let oid = object_id(Kind::Blob, &merged).map_err(git_objects::Error::from)?;
    objects.push((Kind::Blob, merged));
    Ok(tree::Entry {
        mode,
        filename,
        oid,
    })
}

async fn read_blob(
    operation: &crab_remote_git::OperationContext,
    oid: ObjectId,
) -> Result<Vec<u8>, Error> {
    let object = operation
        .read_object(oid)
        .await
        .map_err(git_objects::Error::from)?;
    if object.kind != Kind::Blob
        || object.data.len() > MAX_MERGE_BLOB_BYTES
        || object.data.contains(&0)
    {
        return Err(Error::Conflict);
    }
    Ok(object.data.to_vec())
}

fn entries_by_name(entries: Vec<tree::Entry>) -> BTreeMap<Vec<u8>, tree::Entry> {
    entries
        .into_iter()
        .map(|entry| (entry.filename.to_vec(), entry))
        .collect()
}

fn same_entry(left: Option<&tree::Entry>, right: Option<&tree::Entry>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.mode == right.mode && left.oid == right.oid,
        _ => false,
    }
}
