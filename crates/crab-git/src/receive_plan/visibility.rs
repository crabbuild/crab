//! Exact per-ref reachability evidence for canonical visibility publication.

use super::*;

/// A pinned object source with an optional complete prior ref closure.
///
/// The prior tip and membership answers must come from the same committed,
/// generation-bound proof as `GraphSource`. A union of unrelated ref closures
/// is not a valid prior closure. Sources must bound their own I/O and allocations.
pub trait VisibilitySource: GraphSource {
    fn prior_tip(&self) -> Option<ObjectId>;
    fn in_prior_closure(&mut self, oid: &ObjectId) -> std::result::Result<bool, SourceError>;
}

/// Exact objects to include in a new ref's visibility evidence.
#[derive(Debug, PartialEq, Eq)]
pub enum RefVisibility {
    /// The new graph contains the entire prior closure, plus these sorted objects.
    Additive {
        base: ObjectId,
        added: Vec<ObjectId>,
    },
    /// The sorted complete new closure, replacing any previous visibility.
    Replacement { objects: Vec<ObjectId> },
}

/// Computes exact ref reachability without a clone or local object database.
///
/// Reuses a prior closure only after finding its actual tip in the new graph.
/// Otherwise it walks the complete new graph and returns replacement evidence,
/// removing old objects that are no longer reachable. Gitlinks are not followed.
/// A trusted object's existence alone cannot stop reachability traversal.
/// Call after `validate`, on a blocking worker, with the same pinned source and
/// caller-provided bounds. This proves visibility, not ref policy or publication.
pub fn plan_visibility<S: VisibilitySource, C: Fn() -> bool>(
    incoming: &IncomingPack,
    new: ObjectId,
    source: &mut S,
    limits: GraphLimits,
    cancelled: C,
) -> Result<RefVisibility> {
    let prior = source.prior_tip();
    let mut validator = Validator::new(incoming, source, limits, cancelled);
    validator.step()?;
    if let Some(prior) = prior
        && !validator.prior_contains(prior)?
    {
        return Err(ReceivePlanError::Invalid {
            oid: prior,
            reason: "prior visibility does not contain its ref tip",
        });
    }
    let (objects, prior_members) = validator.visibility_walk(new, prior.is_some())?;
    if let Some(prior) = prior {
        if objects.contains_key(&prior) {
            return Ok(RefVisibility::Additive {
                base: prior,
                added: objects
                    .into_keys()
                    .filter(|oid| !prior_members.contains(oid))
                    .collect(),
            });
        }
        // Pruned members are safe only when their whole prior closure is still
        // reachable. Forced rewrites must expand them so old visibility cannot leak.
        let (objects, _) = validator.visibility_walk(new, false)?;
        return Ok(RefVisibility::Replacement {
            objects: objects.into_keys().collect(),
        });
    }
    Ok(RefVisibility::Replacement {
        objects: objects.into_keys().collect(),
    })
}

impl<S: VisibilitySource, C: Fn() -> bool> Validator<'_, S, C> {
    fn prior_contains(&mut self, oid: ObjectId) -> Result<bool> {
        self.source
            .in_prior_closure(&oid)
            .map_err(|source| ReceivePlanError::Source { oid, source })
    }

    fn visibility_walk(
        &mut self,
        new: ObjectId,
        prune_prior: bool,
    ) -> Result<(BTreeMap<ObjectId, Kind>, HashSet<ObjectId>)> {
        let mut objects = BTreeMap::new();
        let mut prior_members = HashSet::new();
        let mut pending = vec![(new, None)];
        while let Some((oid, expected)) = pending.pop() {
            self.step()?;
            let visited = objects.get(&oid).copied();
            let kind = match visited {
                Some(kind) => kind,
                None if self.incoming.object(&oid).is_some() => self.load(oid)?.kind,
                None => match self.trusted_kind(oid)? {
                    Some(kind) => kind,
                    None => self.load(oid)?.kind,
                },
            };
            if let Some(expected) = expected
                && expected != kind
            {
                return Err(ReceivePlanError::Kind {
                    oid,
                    expected,
                    actual: kind,
                });
            }
            if visited.is_some() {
                continue;
            }
            objects.insert(oid, kind);
            if prune_prior && self.prior_contains(oid)? {
                prior_members.insert(oid);
                continue;
            }
            // A proven blob is a leaf. Commits, trees and tags still need their
            // outgoing edges even if another ref's proof already covers them.
            if kind != Kind::Blob {
                let node = self.load(oid)?;
                if node.kind != kind {
                    return Err(ReceivePlanError::Kind {
                        oid,
                        expected: kind,
                        actual: node.kind,
                    });
                }
                pending.extend(node.links.into_iter().map(|(oid, kind)| (oid, Some(kind))));
            }
        }
        Ok((objects, prior_members))
    }
}
