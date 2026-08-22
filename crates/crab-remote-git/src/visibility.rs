use std::collections::{BTreeMap, HashMap, HashSet};

use gix_hash::ObjectId;
use gix_object::Kind;
use tokio_util::sync::CancellationToken;

use crate::objects::{parse_commit, parse_tag, parse_tree_raw};
use crate::{
    CorruptionStage, EntryKind, Error, GitVisibilityIndex, OperationContext, OperationKind,
    RemoteGitRepository, Result, RevisionError,
};

#[derive(Clone, Copy)]
struct PendingObject {
    oid: ObjectId,
    expected: Option<Kind>,
    corruption_stage: CorruptionStage,
    tag_depth: usize,
}

struct ObjectLinks {
    kind: Kind,
    children: Vec<PendingObject>,
    // A verified tree entry proves blob reachability. Reading blob bodies here
    // would add one storage request per file without strengthening the proof.
    terminals: Vec<ObjectId>,
}

pub(crate) async fn rebuild(
    repository: &RemoteGitRepository,
    pack_index_hash: String,
    cancellation: &CancellationToken,
) -> Result<GitVisibilityIndex> {
    if repository.refs().is_empty() {
        return Ok(GitVisibilityIndex::new(
            repository.generation(),
            pack_index_hash,
            repository.git_validation_digest(),
            BTreeMap::new(),
        ));
    }

    let operation = repository
        .operation(OperationKind::Visibility, cancellation)
        .await?;
    let result = rebuild_with_operation(repository, pack_index_hash, &operation).await;
    operation.finish(result).await
}

async fn rebuild_with_operation(
    repository: &RemoteGitRepository,
    pack_index_hash: String,
    operation: &OperationContext,
) -> Result<GitVisibilityIndex> {
    let maximum = operation.max_logical_objects();
    let mut cache = HashMap::<ObjectId, ObjectLinks>::new();
    let mut total = 0u64;
    let mut refs = BTreeMap::new();

    for reference in &repository.refs().entries {
        let mut objects = HashSet::new();
        let mut pending = vec![PendingObject {
            oid: reference.target,
            expected: None,
            corruption_stage: CorruptionStage::Tag,
            tag_depth: 0,
        }];

        while let Some(next) = pending.pop() {
            operation.ensure_active()?;
            if !objects.insert(next.oid) {
                if next.expected == Some(Kind::Tag) {
                    return Err(Error::Revision {
                        reason: RevisionError::TagCycle,
                    });
                }
                continue;
            }
            total = total.saturating_add(1);
            if total > maximum {
                return Err(Error::LimitExceeded {
                    limit: "visibility proof objects",
                    actual: total,
                    maximum,
                });
            }

            if let Some(cached) = cache.get(&next.oid) {
                ensure_kind(cached.kind, next)?;
                insert_terminals(&mut objects, &cached.terminals, &mut total, maximum)?;
                pending.extend(cached.children.iter().copied());
                continue;
            }

            let object = operation.read_object(next.oid).await?;
            ensure_kind(object.kind, next)?;
            let links = object_links(&object, next.tag_depth, operation)?;
            insert_terminals(&mut objects, &links.terminals, &mut total, maximum)?;
            pending.extend(links.children.iter().copied());
            cache.insert(next.oid, links);
        }

        if reference
            .peeled
            .is_some_and(|peeled| !objects.contains(&peeled))
        {
            return Err(Error::RepositoryState {
                reason: crate::RepositoryStateError::VisibilityProofMismatch,
            });
        }
        let mut objects = objects
            .into_iter()
            .map(|oid| oid.to_hex().to_string())
            .collect::<Vec<_>>();
        objects.sort_unstable();
        refs.insert(reference.name.clone(), objects);
    }

    Ok(GitVisibilityIndex::new(
        repository.generation(),
        pack_index_hash,
        repository.git_validation_digest(),
        refs,
    ))
}

fn insert_terminals(
    objects: &mut HashSet<ObjectId>,
    terminals: &[ObjectId],
    total: &mut u64,
    maximum: u64,
) -> Result<()> {
    for oid in terminals {
        if !objects.insert(*oid) {
            continue;
        }
        *total = total.saturating_add(1);
        if *total > maximum {
            return Err(Error::LimitExceeded {
                limit: "visibility proof objects",
                actual: *total,
                maximum,
            });
        }
    }
    Ok(())
}

fn ensure_kind(actual: Kind, pending: PendingObject) -> Result<()> {
    if pending.expected.is_none_or(|expected| expected == actual) {
        Ok(())
    } else {
        Err(Error::Corrupt {
            stage: pending.corruption_stage,
        })
    }
}

fn object_links(
    object: &crate::RemoteGitObject,
    tag_depth: usize,
    operation: &OperationContext,
) -> Result<ObjectLinks> {
    let (children, terminals) = match object.kind {
        Kind::Commit => {
            let commit = parse_commit(object)?;
            let mut children = Vec::with_capacity(commit.parents.len().saturating_add(1));
            children.push(PendingObject {
                oid: commit.tree,
                expected: Some(Kind::Tree),
                corruption_stage: CorruptionStage::Commit,
                tag_depth: 0,
            });
            children.extend(commit.parents.into_iter().map(|oid| PendingObject {
                oid,
                expected: Some(Kind::Commit),
                corruption_stage: CorruptionStage::Commit,
                tag_depth: 0,
            }));
            (children, Vec::new())
        }
        Kind::Tree => {
            let mut children = Vec::new();
            let mut terminals = Vec::new();
            for entry in parse_tree_raw(object)? {
                match entry.kind {
                    EntryKind::Submodule => {}
                    EntryKind::Tree => children.push(PendingObject {
                        oid: entry.oid,
                        expected: Some(Kind::Tree),
                        corruption_stage: CorruptionStage::Tree,
                        tag_depth: 0,
                    }),
                    EntryKind::Blob | EntryKind::Symlink => terminals.push(entry.oid),
                }
            }
            (children, terminals)
        }
        Kind::Blob => (Vec::new(), Vec::new()),
        Kind::Tag => {
            if tag_depth >= operation.object_limits().max_tag_depth {
                return Err(Error::Revision {
                    reason: RevisionError::TagDepth,
                });
            }
            let tag = parse_tag(object)?;
            (
                vec![PendingObject {
                    oid: tag.target,
                    expected: Some(tag.target_kind),
                    corruption_stage: CorruptionStage::Tag,
                    tag_depth: tag_depth.saturating_add(1),
                }],
                Vec::new(),
            )
        }
    };
    Ok(ObjectLinks {
        kind: object.kind,
        children,
        terminals,
    })
}
