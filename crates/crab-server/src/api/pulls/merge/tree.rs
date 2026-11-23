use std::collections::{BTreeMap, HashMap};

use crab_remote::objects::{self, encode_tree, object_id};
use futures_util::future::BoxFuture;
use gix_hash::ObjectId;
use gix_object::{Kind, bstr::BString, tree};

use super::{MergeMethod, conflicts, rebase};
use crate::{auth::Identity, git::objects::commit_bytes};
use crab_remote_git::{OperationContext, RemoteGitObject, RemoteGitSnapshot, Revision};

const MAX_MERGE_BLOB_BYTES: usize = 8 * 1024 * 1024;

pub(in crate::api::pulls) struct Plan {
    pub oid: ObjectId,
    pub objects: Vec<(Kind, Vec<u8>)>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("the pull request branches do not have the recorded base in their history")]
    History,
    #[error("the pull request changes conflict")]
    Conflict,
    #[error("{0}")]
    Resolution(&'static str),
    #[error("the pull request cannot be safely rebased")]
    Rebase,
    #[error(transparent)]
    Object(#[from] objects::Error),
}

pub(in crate::api::pulls) struct Strategy<'a> {
    pub method: MergeMethod,
    pub actor: &'a Identity,
    pub message: &'a str,
    pub seconds: u64,
    pub allow_empty_rebase: bool,
}

pub(super) struct Readers<'a> {
    pub base: &'a OperationContext,
    pub head: &'a OperationContext,
}

impl Readers<'_> {
    async fn object(&self, oid: ObjectId) -> Result<RemoteGitObject, Error> {
        // The caller derived the ID from the two captured commit graphs.
        // Missing in one reader can mean the other graph owns this object.
        let result = match self.base.read_object(oid).await {
            Err(crab_remote_git::Error::ObjectNotFound { .. })
                if !std::ptr::eq(self.base, self.head) =>
            {
                self.head.read_object(oid).await
            }
            result => result,
        };
        result.map_err(objects::Error::from).map_err(Error::from)
    }
}

pub(super) async fn build(
    git: &super::super::git::PullGit,
    readers: &Readers<'_>,
    current: ObjectId,
    other: ObjectId,
    strategy: Strategy<'_>,
    conflicts: conflicts::Mode,
) -> Result<Plan, Error> {
    let (base, head, ancestor) = comparison(git, readers, current, other).await?;
    if strategy.method == MergeMethod::Rebase {
        return rebase::build(git.head(), readers, &base, &head, &ancestor, strategy).await;
    }
    let mut generated = Generated {
        conflicts,
        ..Generated::default()
    };
    let tree = merged_tree(
        readers,
        ancestor.root_tree_oid(),
        &base,
        &head,
        &mut generated,
    )
    .await?;
    generated.conflicts.check_complete()?;
    let parents = [current, other];
    let parents = if strategy.method == MergeMethod::Squash {
        &parents[..1]
    } else {
        &parents
    };
    let commit = commit_bytes(
        tree,
        parents,
        strategy.actor,
        strategy.message,
        strategy.seconds,
    )?;
    let oid = generated.add(Kind::Commit, commit)?;
    Ok(generated.plan(oid))
}

pub(super) async fn preview_conflicts(
    git: &super::super::git::PullGit,
    readers: &Readers<'_>,
    current: ObjectId,
    other: ObjectId,
) -> Result<Vec<conflicts::File>, Error> {
    let (base, head, ancestor) = comparison(git, readers, current, other).await?;
    let mut generated = Generated {
        conflicts: conflicts::Mode::Preview(vec![]),
        ..Generated::bounded()
    };
    merged_tree(
        readers,
        ancestor.root_tree_oid(),
        &base,
        &head,
        &mut generated,
    )
    .await?;
    let conflicts::Mode::Preview(files) = generated.conflicts else {
        return Err(Error::History);
    };
    Ok(files)
}

async fn comparison(
    git: &super::super::git::PullGit,
    readers: &Readers<'_>,
    current: ObjectId,
    other: ObjectId,
) -> Result<(RemoteGitSnapshot, RemoteGitSnapshot, RemoteGitSnapshot), Error> {
    let base = git
        .base
        .snapshot(&Revision::Commit(current), readers.base)
        .await
        .map_err(objects::Error::from)?;
    let head = git
        .head()
        .snapshot(&Revision::Commit(other), readers.head)
        .await
        .map_err(objects::Error::from)?;
    let bases = base
        .merge_bases_across_captures(&head, readers.base, readers.head)
        .await
        .map_err(objects::Error::from)?;
    // A criss-cross merge needs a virtual base tree. Choosing just one can lose changes.
    let [ancestor] = bases.as_slice() else {
        return Err(Error::History);
    };
    let ancestor = git
        .base
        .snapshot(&Revision::Commit(*ancestor), readers.base)
        .await
        .map_err(objects::Error::from)?;
    Ok((base, head, ancestor))
}

pub(super) async fn merged_tree(
    readers: &Readers<'_>,
    ancestor_tree: ObjectId,
    base: &RemoteGitSnapshot,
    head: &RemoteGitSnapshot,
    generated: &mut Generated,
) -> Result<ObjectId, Error> {
    merge_tree(
        readers,
        Some(ancestor_tree),
        base.root_tree_oid(),
        head.root_tree_oid(),
        generated,
    )
    .await
}

#[derive(Default)]
pub(super) struct Generated {
    objects: Vec<(Kind, Vec<u8>)>,
    by_oid: HashMap<ObjectId, usize>,
    bytes: usize,
    bounded: bool,
    conflicts: conflicts::Mode,
}

impl Generated {
    pub(super) fn bounded() -> Self {
        Self {
            bounded: true,
            ..Self::default()
        }
    }

    pub(super) fn add(&mut self, kind: Kind, data: Vec<u8>) -> Result<ObjectId, Error> {
        let oid = object_id(kind, &data).map_err(objects::Error::from)?;
        self.objects.push((kind, data));
        self.index(oid)?;
        Ok(oid)
    }

    fn index(&mut self, oid: ObjectId) -> Result<(), Error> {
        let Some((_, data)) = self.objects.last() else {
            return Err(Error::History);
        };
        self.bytes = self.bytes.saturating_add(data.len());
        if self.bounded && (self.bytes > 64 * 1024 * 1024 || self.objects.len() > 16_384) {
            return Err(Error::Rebase);
        }
        self.by_oid.insert(oid, self.objects.len() - 1);
        Ok(())
    }

    pub(super) fn plan(self, oid: ObjectId) -> Plan {
        Plan {
            oid,
            objects: self.objects,
        }
    }

    fn get(&self, oid: ObjectId) -> Option<&(Kind, Vec<u8>)> {
        self.by_oid
            .get(&oid)
            .and_then(|index| self.objects.get(*index))
    }

    async fn tree(&self, readers: &Readers<'_>, oid: ObjectId) -> Result<Vec<tree::Entry>, Error> {
        let object;
        let (kind, data) = if let Some((kind, data)) = self.get(oid) {
            (*kind, data.as_slice())
        } else {
            object = readers.object(oid).await?;
            (object.kind, object.data.as_ref())
        };
        if kind != Kind::Tree {
            return Err(Error::Conflict);
        }
        Ok(gix_object::TreeRef::from_bytes(data, gix_hash::Kind::Sha1)
            .map_err(objects::Error::from)?
            .into_owned()
            .entries)
    }
}

pub(super) fn merge_tree<'a>(
    readers: &'a Readers<'a>,
    ancestor_oid: Option<ObjectId>,
    current_oid: ObjectId,
    other_oid: ObjectId,
    objects: &'a mut Generated,
) -> BoxFuture<'a, Result<ObjectId, Error>> {
    Box::pin(async move {
        merge_tree_at(
            readers,
            ancestor_oid,
            current_oid,
            other_oid,
            objects,
            &mut Vec::new(),
        )
        .await
    })
}

fn merge_tree_at<'a>(
    readers: &'a Readers<'a>,
    ancestor_oid: Option<ObjectId>,
    current_oid: ObjectId,
    other_oid: ObjectId,
    objects: &'a mut Generated,
    path: &'a mut Vec<u8>,
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
            Some(oid) => objects.tree(readers, oid).await?,
            None => vec![],
        };
        let current = objects.tree(readers, current_oid).await?;
        let other = objects.tree(readers, other_oid).await?;
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
            let parent = path.len();
            if parent != 0 {
                path.push(b'/');
            }
            path.extend(&name);
            let entry = merge_entry(
                readers,
                ancestor.get(&name),
                current.get(&name),
                other.get(&name),
                objects,
                path,
            )
            .await?;
            path.truncate(parent);
            if let Some(mut entry) = entry {
                entry.filename = BString::from(name);
                merged.push(entry);
            }
        }
        let oid = encode_tree(merged, &mut objects.objects)?;
        objects.index(oid)?;
        Ok(oid)
    })
}

fn merge_entry<'a>(
    readers: &'a Readers<'a>,
    ancestor: Option<&'a tree::Entry>,
    current: Option<&'a tree::Entry>,
    other: Option<&'a tree::Entry>,
    objects: &'a mut Generated,
    path: &'a mut Vec<u8>,
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
                let oid = merge_tree_at(
                    readers,
                    ancestor.map(|entry| entry.oid),
                    current.oid,
                    other.oid,
                    objects,
                    path,
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
                merge_blob(readers, ancestor, current, other, objects, path)
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
    readers: &Readers<'_>,
    ancestor: &tree::Entry,
    current: &tree::Entry,
    other: &tree::Entry,
    objects: &mut Generated,
    path: &[u8],
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
    let ancestor_bytes = read_blob(readers, objects, ancestor.oid).await?;
    let current_bytes = read_blob(readers, objects, current.oid).await?;
    let other_bytes = read_blob(readers, objects, other.oid).await?;
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
    let labels = gix_merge::blob::builtin_driver::text::Labels {
        current: Some(gix_object::bstr::BStr::new(b"PR head")),
        other: Some(gix_object::bstr::BStr::new(b"Base branch")),
        ancestor: None,
    };
    let resolution = merge.run(&mut merged, labels, options.conflict);
    if resolution == gix_merge::blob::Resolution::Conflict {
        if matches!(objects.conflicts, conflicts::Mode::Reject)
            || [ancestor, current, other].iter().any(|entry| {
                !matches!(
                    entry.mode.kind(),
                    tree::EntryKind::Blob | tree::EntryKind::BlobExecutable
                )
            })
            || [&ancestor_bytes, &current_bytes, &other_bytes]
                .iter()
                .any(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err())
        {
            return Err(Error::Conflict);
        }
        merged = objects.conflicts.resolve(path, merged)?;
    }
    let oid = objects.add(Kind::Blob, merged)?;
    Ok(tree::Entry {
        mode,
        filename,
        oid,
    })
}

async fn read_blob(
    readers: &Readers<'_>,
    generated: &Generated,
    oid: ObjectId,
) -> Result<Vec<u8>, Error> {
    if let Some((kind, bytes)) = generated.get(oid) {
        return checked_blob(*kind, bytes);
    }
    let object = readers.object(oid).await?;
    checked_blob(object.kind, &object.data)
}

fn checked_blob(kind: Kind, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if kind != Kind::Blob || bytes.len() > MAX_MERGE_BLOB_BYTES || bytes.contains(&0) {
        return Err(Error::Conflict);
    }
    Ok(bytes.to_vec())
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
