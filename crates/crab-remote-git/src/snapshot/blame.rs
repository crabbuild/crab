use super::*;

/// Shared immutable attribution retaining its runtime memory reservation.
///
/// Cloning retains the same result. Explicitly cloning the borrowed `Blame`
/// transfers a separately allocated result to the caller's memory envelope.
#[derive(Clone)]
pub struct SharedBlame(Arc<crate::resident::Retained<Blame>>);

impl SharedBlame {
    pub(crate) fn new(value: Blame, reservation: crate::resident::ReadReservation) -> Self {
        Self(Arc::new(crate::resident::Retained::new(value, reservation)))
    }
}

impl AsRef<Blame> for SharedBlame {
    fn as_ref(&self) -> &Blame {
        &self.0.value
    }
}

impl std::ops::Deref for SharedBlame {
    type Target = Blame;
    fn deref(&self) -> &Blame {
        self.as_ref()
    }
}

impl std::fmt::Debug for SharedBlame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl RemoteGitSnapshot {
    /// Attribute a file, returning a caller-owned copy of its complete result.
    ///
    /// Computation and budgets share the managed result path. The returned
    /// owned vectors belong to the caller, outside the runtime's byte envelope.
    pub async fn blame(&self, path: &GitPath, operation: &OperationContext) -> Result<Blame> {
        Ok(self.shared_blame(path, operation).await?.as_ref().clone())
    }

    /// Attribute a file while retaining the shared result's memory ownership.
    ///
    /// Callers authorize the pinned snapshot and retain remote-object authority
    /// until operation cleanup and runtime work have drained.
    pub async fn shared_blame(
        &self,
        path: &GitPath,
        operation: &OperationContext,
    ) -> Result<SharedBlame> {
        self.ensure_operation(operation)?;
        operation.ensure_active()?;
        if let Some(cached) = operation.cached_blame(self.commit_oid, path).await {
            operation.charge_cached(cached.usage).await?;
            return Ok(cached.value);
        }
        let snapshot = self.clone();
        let path = path.clone();
        let key = operation.blame_key(self.commit_oid, &path);
        self.repository
            .runtime
            .read_blame_singleflight(key, operation, move |cancellation, shared| async move {
                let producer = OperationContext::open_shared(
                    snapshot.repository.clone(),
                    &cancellation,
                    shared,
                )
                .await?;
                let result = snapshot.compute_blame(&path, &producer).await;
                producer.finish(result).await
            })
            .await
    }

    /// Attribute every line in one ordinary UTF-8 file through first parents.
    ///
    /// The result is complete or fails with a resource limit. Rename following,
    /// merge-parent attribution, binary blobs, and pointer materialization are
    /// intentionally outside this bounded object-store reader operation.
    async fn compute_blame(
        &self,
        path: &GitPath,
        operation: &OperationContext,
    ) -> Result<SharedBlame> {
        self.ensure_operation(operation)?;
        operation.ensure_active()?;
        if let Some(cached) = operation.cached_blame(self.commit_oid, path).await {
            operation.charge_cached(cached.usage).await?;
            return Ok(cached.value);
        }
        let _producer =
            crate::metrics::BlameProducerObservation::new(operation.runtime().metrics());
        let budget_before = operation.budget_usage().await;
        let head_blob = read_blame_blob(self, path, operation).await?;
        let path_index = self.repository.path_state(operation).await?;
        let head_oid = match &path_index {
            Some(index) => index
                .last_change(self.commit_oid, path, operation)
                .await?
                .ok_or(Error::Corrupt {
                    stage: CorruptionStage::PathState,
                })?,
            None => self.commit_oid,
        };
        let head_commit = if path_index.is_some() {
            history_commit(head_oid, operation).await?
        } else {
            operation.read_commit_shared(head_oid).await?
        };
        if path_index.is_some() {
            let entry =
                entries_at_trees(std::slice::from_ref(&head_commit), path, operation).await?;
            if entry[0].as_ref().map(|entry| entry.oid) != Some(head_blob.metadata.oid) {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::PathState,
                });
            }
        }
        let mut current_oid = head_blob.metadata.oid;
        let mut current_bytes = head_blob.bytes;
        let current_line_count = line_count(&current_bytes, operation.cancellation())?;
        operation
            .charge(BudgetDimension::BlameLines, current_line_count as u64)
            .await?;
        let row_bytes = mem::size_of::<Range<usize>>() + mem::size_of::<Option<usize>>();
        let _origin_memory = operation
            .runtime()
            .resident
            .reserve(current_line_count.saturating_mul(mem::size_of::<ObjectId>()))?;
        let mut _line_memory = operation
            .runtime()
            .resident
            .reserve(current_line_count.saturating_mul(row_bytes))?;
        let mut history_memory = operation.runtime().resident.reserve(0)?;
        let history_slot_bytes =
            (mem::size_of::<ObjectId>() + mem::size_of::<crate::objects::SharedCommit>()) * 4;
        history_memory.grow(history_slot_bytes)?;
        let mut current_lines =
            line_ranges(&current_bytes, current_line_count, operation.cancellation())?;

        let mut origins = Vec::new();
        origins
            .try_reserve_exact(current_lines.len())
            .map_err(|source| Error::Allocation {
                requested: current_lines
                    .len()
                    .saturating_mul(mem::size_of::<ObjectId>()),
                source,
            })?;
        origins.resize(current_lines.len(), head_commit.oid);
        let mut tracked = Vec::new();
        tracked
            .try_reserve_exact(current_lines.len())
            .map_err(|source| Error::Allocation {
                requested: current_lines
                    .len()
                    .saturating_mul(mem::size_of::<Option<usize>>()),
                source,
            })?;
        tracked.extend((0..current_lines.len()).map(Some));
        let mut commits = HashMap::new();
        commits.insert(head_commit.oid, head_commit.clone());
        let mut current_commit = head_commit;
        let mut visited = HashSet::new();
        visited.insert(current_commit.oid);

        'history: while let Some(parent_oid) = current_commit.parents.first().copied() {
            let parent_commits = match &path_index {
                Some(index) => {
                    let Some(changed) = index.last_change(parent_oid, path, operation).await?
                    else {
                        break;
                    };
                    let commits = vec![history_commit(changed, operation).await?];
                    if let Some(graph) = self.commit_graph.as_deref()
                        && !graph.parents_match(&commits, operation).await?
                    {
                        return Err(Error::Corrupt {
                            stage: CorruptionStage::PathState,
                        });
                    }
                    commits
                }
                None => {
                    blame_parent_commits(parent_oid, self.commit_graph.as_deref(), operation)
                        .await?
                }
            };
            for parent_commit in &parent_commits {
                if visited.contains(&parent_commit.oid) {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::Commit,
                    });
                }
                history_memory.grow(history_slot_bytes)?;
                visited.insert(parent_commit.oid);
            }
            let parent_entries = entries_at_trees(&parent_commits, path, operation).await?;
            for (parent_commit, parent_entry) in parent_commits.into_iter().zip(parent_entries) {
                let Some(parent_entry) = parent_entry.filter(|entry| entry.kind == EntryKind::Blob)
                else {
                    break 'history;
                };
                if parent_entry.oid == current_oid {
                    let mut attributed = false;
                    for final_line in tracked.iter().flatten() {
                        origins[*final_line] = parent_commit.oid;
                        attributed = true;
                    }
                    if attributed {
                        commits.insert(parent_commit.oid, parent_commit.clone());
                    }
                    if !origins.contains(&current_commit.oid) {
                        commits.remove(&current_commit.oid);
                    }
                    current_commit = parent_commit;
                    continue;
                }
                let Some(parent_blob) =
                    optional_blame_blob_for_entry(parent_entry, operation).await?
                else {
                    break 'history;
                };
                let mut attributed = false;
                let parent_bytes = parent_blob.bytes;
                let parent_line_count = line_count(&parent_bytes, operation.cancellation())?;
                let parent_memory = operation
                    .runtime()
                    .resident
                    .reserve(parent_line_count.saturating_mul(row_bytes))?;
                let parent_lines =
                    line_ranges(&parent_bytes, parent_line_count, operation.cancellation())?;
                let (equal_prefix, equal_suffix) = matching_ends(
                    &parent_bytes,
                    &parent_lines,
                    &current_bytes,
                    &current_lines,
                    operation.cancellation(),
                )?;
                let comparison_cells = comparison_cells(
                    parent_line_count - equal_prefix - equal_suffix,
                    current_lines.len() - equal_prefix - equal_suffix,
                )?;
                operation
                    .charge(BudgetDimension::BlameComparisons, comparison_cells)
                    .await?;
                // Both blobs and the comparison budget are validated before a
                // cache hit. Reusing line mappings cannot authorize an object or
                // bypass a stricter operation limit.
                let matches = match operation
                    .cached_blame_comparison(parent_blob.metadata.oid, current_oid)
                    .await
                {
                    Some(matches) => matches,
                    None => {
                        let matches = git_line_matches(
                            operation.runtime(),
                            parent_bytes.clone(),
                            parent_line_count,
                            current_bytes.clone(),
                            current_lines.len(),
                            operation.cancellation().clone(),
                        )
                        .await?;
                        operation.ensure_active()?;
                        operation
                            .insert_blame_comparison(
                                parent_blob.metadata.oid,
                                current_oid,
                                matches.clone(),
                            )
                            .await;
                        matches
                    }
                };
                operation.ensure_active()?;
                let mut parent_tracked = Vec::new();
                parent_tracked
                    .try_reserve_exact(parent_lines.len())
                    .map_err(|source| Error::Allocation {
                        requested: parent_lines
                            .len()
                            .saturating_mul(mem::size_of::<Option<usize>>()),
                        source,
                    })?;
                parent_tracked.resize(parent_lines.len(), None);
                for &(parent_line, current_line) in matches.iter() {
                    if let Some(final_line) = tracked[current_line] {
                        origins[final_line] = parent_commit.oid;
                        parent_tracked[parent_line] = Some(final_line);
                        attributed = true;
                    }
                }
                if attributed {
                    commits.insert(parent_commit.oid, parent_commit.clone());
                }
                // Keep metadata only for commits that still own a final line.
                if !origins.contains(&current_commit.oid) {
                    commits.remove(&current_commit.oid);
                }
                if parent_tracked.iter().all(Option::is_none) {
                    break 'history;
                }
                current_commit = parent_commit;
                current_oid = parent_blob.metadata.oid;
                current_bytes = parent_bytes;
                current_lines = parent_lines;
                tracked = parent_tracked;
                _line_memory = parent_memory;
            }
        }

        let (ranges, reservation) = blame_ranges(path, &origins, &commits, operation).await?;
        let value = Blame {
            commit: self.commit_oid,
            path: path.clone(),
            ranges,
        };
        let blame = SharedBlame::new(value, reservation);
        let depth = u64::try_from(path.components().count()).map_err(|_| Error::LimitExceeded {
            limit: "traversal depth",
            actual: u64::MAX,
            maximum: u64::MAX,
        })?;
        let usage = operation
            .budget_usage()
            .await
            .semantic_delta(budget_before, depth);
        operation
            .insert_blame(self.commit_oid, path.clone(), blame.clone(), usage)
            .await;
        Ok(blame)
    }
}

async fn history_commit(
    oid: ObjectId,
    operation: &OperationContext,
) -> Result<crate::objects::SharedCommit> {
    operation.charge(BudgetDimension::HistoryCommits, 1).await?;
    operation.read_commit_shared(oid).await
}
