use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crab_git::{
    incoming_pack::{self, BaseObject, IncomingPack, PreparedPack, ReceiveLimits},
    receive_plan::{
        self, GraphLimits, GraphSource, RefPolicy, RefUpdate, RefVisibility, ValidatedRefUpdates,
        VisibilitySource,
    },
};
use crab_metadata::{
    git_object_locator::GitObjectKind,
    git_visibility::{GitCatalogVisibilityIndex, GitVisibilityEdit},
};
use crab_remote_git::{OperationContext, OperationKind, RemoteGitRepository};
use gix_hash::ObjectId;
use gix_object::Kind;
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result};

const GRAPH_LIMITS: GraphLimits = GraphLimits {
    max_ref_updates: 1024,
    max_graph_steps: 1_000_000,
    max_object_bytes: 64 * 1024 * 1024,
    max_read_bytes: 512 * 1024 * 1024,
};

pub(super) struct Prepared {
    pub plan: ValidatedRefUpdates,
    pub visibility: BTreeMap<String, GitVisibilityEdit>,
    pub pack: Option<PreparedPack>,
}

struct Source<'a> {
    operation: &'a OperationContext,
    proof: Option<GitCatalogVisibilityIndex>,
    refs: Vec<String>,
    prior: Option<(String, ObjectId)>,
    handle: tokio::runtime::Handle,
}

type SourceResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

impl Source<'_> {
    fn ordinal(&self, oid: &ObjectId) -> SourceResult<Option<u32>> {
        if self.proof.is_none() {
            return Ok(None);
        }
        Ok(self
            .handle
            .block_on(self.operation.catalog_object_ordinals(&[*oid]))?
            .into_iter()
            .next()
            .flatten())
    }
    fn visible(&self, oid: &ObjectId) -> SourceResult<bool> {
        Ok(self.ordinal(oid)?.is_some_and(|ordinal| {
            self.proof.as_ref().is_some_and(|proof| {
                proof.contains_ordinal_for_refs(self.refs.iter().map(String::as_str), ordinal)
            })
        }))
    }
}

impl GraphSource for Source<'_> {
    fn trusted_kind(&mut self, oid: &ObjectId) -> SourceResult<Option<Kind>> {
        if !self.visible(oid)? {
            return Ok(None);
        }
        let bytes = oid.as_bytes().try_into()?;
        Ok(self
            .handle
            .block_on(self.operation.catalog_object_kinds(&[bytes]))?
            .into_iter()
            .next()
            .flatten()
            .map(|kind| match kind {
                GitObjectKind::Commit => Kind::Commit,
                GitObjectKind::Tree => Kind::Tree,
                GitObjectKind::Blob => Kind::Blob,
                GitObjectKind::Tag => Kind::Tag,
            }))
    }
    fn read(&mut self, oid: &ObjectId) -> SourceResult<Option<BaseObject>> {
        // A locator hit alone cannot authorize a dangling object or thin base.
        if !self.visible(oid)? {
            return Ok(None);
        }
        let object = self.handle.block_on(self.operation.read_object(*oid))?;
        Ok(Some(BaseObject {
            kind: object.kind,
            data: object.data.to_vec(),
        }))
    }
}

impl VisibilitySource for Source<'_> {
    fn prior_tip(&self) -> Option<ObjectId> {
        self.prior.as_ref().map(|(_, oid)| *oid)
    }
    fn in_prior_closure(&mut self, oid: &ObjectId) -> SourceResult<bool> {
        let Some((name, _)) = &self.prior else {
            return Ok(false);
        };
        Ok(self.ordinal(oid)?.is_some_and(|ordinal| {
            self.proof
                .as_ref()
                .is_some_and(|proof| proof.contains_ordinal_in_ref(name, ordinal))
        }))
    }
}

pub(super) async fn prepare(
    repository: RemoteGitRepository,
    directory: std::path::PathBuf,
    mut input: BufReader<File>,
    updates: Vec<RefUpdate>,
    cancel: &CancellationToken,
) -> Result<Prepared> {
    let default_branch = repository
        .refs()
        .head
        .as_ref()
        .map(|head| head.name.clone());
    let base: BTreeMap<_, _> = repository
        .refs()
        .entries
        .iter()
        .map(|reference| (reference.name.clone(), reference.target))
        .collect();
    let proof = if base.is_empty() {
        None
    } else {
        Some(repository.catalog_visibility_index(cancel).await?)
    };
    let operation = repository
        .operation(OperationKind::Repository, cancel)
        .await?;
    let handle = tokio::runtime::Handle::current();
    let cancel = cancel.clone();
    let flag = Arc::new(AtomicBool::new(false));
    let watched = cancel.clone();
    let watched_flag = Arc::clone(&flag);
    let watcher = tokio::spawn(async move {
        watched.cancelled().await;
        watched_flag.store(true, Ordering::Release);
    });
    let work = tokio::task::spawn_blocking(move || {
        let mut source = Source {
            operation: &operation,
            proof,
            refs: base.keys().cloned().collect(),
            prior: None,
            handle: handle.clone(),
        };
        let result = (|| {
            super::check_cancelled(&cancel)?;
            let incoming = if input.fill_buf()?.is_empty()
                && updates.iter().all(|update| update.new.is_none())
            {
                IncomingPack::empty(&directory)?
            } else {
                incoming_pack::quarantine(
                    &mut input,
                    &directory,
                    ReceiveLimits {
                        max_pack_bytes: super::MAX_BODY,
                        max_objects: 1_000_000,
                        max_object_bytes: GRAPH_LIMITS.max_object_bytes,
                        max_inflated_bytes: 8 * 1024 * 1024 * 1024,
                        max_delta_depth: 128,
                    },
                    || cancel.is_cancelled(),
                    |oid| source.read(oid),
                )?
            };
            let plan = receive_plan::validate(
                &incoming,
                &base,
                &updates,
                |name| RefPolicy {
                    allow_delete: default_branch.as_deref() != Some(name),
                    allow_non_fast_forward: false,
                },
                &mut source,
                GRAPH_LIMITS,
                || cancel.is_cancelled(),
            )?;
            let mut visibility = BTreeMap::new();
            let mut count = 0usize;
            for update in &updates {
                let Some(new) = update.new else {
                    continue;
                };
                source.prior = update.old.map(|old| (update.name.clone(), old));
                let proof = receive_plan::plan_visibility(
                    &incoming,
                    new,
                    &mut source,
                    GRAPH_LIMITS,
                    || cancel.is_cancelled(),
                )?;
                let old = update.old.map(|oid| oid.to_string());
                let evidence = match proof {
                    RefVisibility::Additive { added, .. } => GitVisibilityEdit::from_delta_objects(
                        old,
                        new.to_string(),
                        added.into_iter().map(|oid| oid.to_string()).collect(),
                        vec![],
                    ),
                    RefVisibility::Replacement { objects } => {
                        GitVisibilityEdit::from_replacement_objects(
                            old,
                            new.to_string(),
                            objects.into_iter().map(|oid| oid.to_string()).collect(),
                        )
                    }
                };
                count = count.saturating_add(evidence.added.len());
                if count > GRAPH_LIMITS.max_graph_steps {
                    return Err(ReceiveError::Request(
                        "Combined ref visibility exceeds one million objects",
                    ));
                }
                visibility.insert(update.name.clone(), evidence);
            }
            let pack = incoming.prepare(&directory, super::MAX_BODY, &flag)?;
            Ok(Prepared {
                plan,
                visibility,
                pack,
            })
        })();
        drop(source);
        let close = handle.block_on(operation.finish(Ok(())));
        match (result, close) {
            (result, Ok(())) => result,
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(operation), Err(close)) => Err(ReceiveError::Close {
                operation: Box::new(operation),
                close,
            }),
        }
    })
    .await;
    watcher.abort();
    work?
}
