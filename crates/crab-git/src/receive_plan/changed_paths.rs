use super::*;

#[derive(Clone)]
struct PathHash {
    hasher: blake3::Hasher,
    empty: bool,
}

impl PathHash {
    fn root() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            empty: true,
        }
    }

    fn child(&self, name: &[u8]) -> Self {
        let mut child = self.clone();
        if !child.empty {
            child.hasher.update(b"/");
        }
        child.hasher.update(name);
        child.empty = false;
        child
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl<S: GraphSource, C: Fn() -> bool> Validator<'_, S, C> {
    pub(super) fn changed_path_hashes(
        &mut self,
        updates: &[RefUpdate],
    ) -> Result<BTreeSet<[u8; 32]>> {
        let mut pending = Vec::new();
        for update in updates {
            let Some(mut oid) = update.new else { continue };
            if self.trusted_kind(oid)?.is_some() {
                continue;
            }
            loop {
                let node = self.load(oid)?;
                if node.kind != Kind::Tag {
                    if node.kind == Kind::Commit {
                        pending.push(oid);
                    }
                    break;
                }
                oid = node
                    .links
                    .first()
                    .ok_or(ReceivePlanError::Invalid {
                        oid,
                        reason: "tag has no target",
                    })?
                    .0;
                if self.trusted_kind(oid)?.is_some() {
                    break;
                }
            }
        }

        let mut paths = BTreeSet::new();
        let mut commits = HashSet::new();
        // A final-tree comparison misses a changed path reverted by a later commit
        // in the same push. Inspect every commit until committed visibility begins.
        while let Some(oid) = pending.pop() {
            self.step()?;
            if !commits.insert(oid) || self.trusted_kind(oid)?.is_some() {
                continue;
            }
            let node = self.load(oid)?;
            if node.kind != Kind::Commit {
                return Err(ReceivePlanError::Kind {
                    oid,
                    expected: Kind::Commit,
                    actual: node.kind,
                });
            }
            let tree = node
                .links
                .first()
                .ok_or(ReceivePlanError::Invalid {
                    oid,
                    reason: "commit has no tree",
                })?
                .0;
            let parents = node
                .links
                .iter()
                .filter_map(|(oid, kind)| (*kind == Kind::Commit).then_some(*oid))
                .collect::<Vec<_>>();
            if parents.is_empty() {
                self.collect_tree_changes(None, Some(tree), PathHash::root(), &mut paths)?;
            } else {
                for parent in &parents {
                    let parent_node = self.load(*parent)?;
                    if parent_node.kind != Kind::Commit {
                        return Err(ReceivePlanError::Kind {
                            oid: *parent,
                            expected: Kind::Commit,
                            actual: parent_node.kind,
                        });
                    }
                    let parent_tree = parent_node
                        .links
                        .first()
                        .ok_or(ReceivePlanError::Invalid {
                            oid: *parent,
                            reason: "commit has no tree",
                        })?
                        .0;
                    self.collect_tree_changes(
                        Some(parent_tree),
                        Some(tree),
                        PathHash::root(),
                        &mut paths,
                    )?;
                }
            }
            pending.extend(parents);
        }
        Ok(paths)
    }

    fn collect_tree_changes(
        &mut self,
        old: Option<ObjectId>,
        new: Option<ObjectId>,
        prefix: PathHash,
        paths: &mut BTreeSet<[u8; 32]>,
    ) -> Result<()> {
        let mut pending = vec![(old, new, prefix)];
        while let Some((old, new, prefix)) = pending.pop() {
            self.step()?;
            if old.is_some() && old == new {
                continue;
            }
            let old_entries = self.tree_entries(old)?;
            let new_entries = self.tree_entries(new)?;
            let names = old_entries
                .keys()
                .chain(new_entries.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for name in names {
                self.step()?;
                let old = old_entries.get(&name);
                let new = new_entries.get(&name);
                let path = prefix.child(&name);
                match (old, new) {
                    (Some(old), Some(new)) if old.mode == new.mode && old.oid == new.oid => {}
                    (Some(old), Some(new)) if is_tree(old) && is_tree(new) => {
                        pending.push((Some(old.oid), Some(new.oid), path));
                    }
                    (Some(old), Some(new)) => {
                        self.collect_entry(old, path.clone(), paths, &mut pending, true);
                        self.collect_entry(new, path, paths, &mut pending, false);
                    }
                    (Some(old), None) => {
                        self.collect_entry(old, path, paths, &mut pending, true);
                    }
                    (None, Some(new)) => {
                        self.collect_entry(new, path, paths, &mut pending, false);
                    }
                    (None, None) => {}
                }
            }
        }
        Ok(())
    }

    fn tree_entries(&mut self, oid: Option<ObjectId>) -> Result<BTreeMap<Vec<u8>, TreeEntry>> {
        let Some(oid) = oid else {
            return Ok(BTreeMap::new());
        };
        let node = self.load(oid)?;
        if node.kind != Kind::Tree {
            return Err(ReceivePlanError::Kind {
                oid,
                expected: Kind::Tree,
                actual: node.kind,
            });
        }
        Ok(node
            .tree
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect())
    }

    fn collect_entry(
        &self,
        entry: &TreeEntry,
        path: PathHash,
        paths: &mut BTreeSet<[u8; 32]>,
        pending: &mut Vec<(Option<ObjectId>, Option<ObjectId>, PathHash)>,
        old: bool,
    ) {
        if is_tree(entry) {
            let pair = if old {
                (Some(entry.oid), None, path)
            } else {
                (None, Some(entry.oid), path)
            };
            pending.push(pair);
        } else {
            paths.insert(path.finish());
        }
    }
}

fn is_tree(entry: &TreeEntry) -> bool {
    entry.mode == 0o040000
}
