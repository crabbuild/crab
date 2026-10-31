//! Bounded checkpoint ownership for published checkpoints during sustained writes.
use super::*;

pub(super) struct RetainedPhysicalCheckpoint {
    checkpoint: crab_metadata::capsule_protocol::LayeredCheckpoint,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl CheckpointScheduler {
    pub(super) fn retain_checkpoint(
        &self,
        checkpoint: crab_metadata::capsule_protocol::LayeredCheckpoint,
    ) -> Option<RetainedPhysicalCheckpoint> {
        let bytes = u32::try_from(checkpoint.retained_metadata_bytes()?).ok()?;
        let available_bytes = self.manifest_bytes.available_permits();
        let permit = Arc::clone(&self.manifest_bytes)
            .try_acquire_many_owned(bytes)
            .ok();
        tracing::info!(
            phase = "metadata_retention",
            elapsed_ms = 0_u64,
            required_bytes = u64::from(bytes),
            available_bytes,
            retained = u64::from(permit.is_some()),
            "S3 physical checkpoint metadata retention"
        );
        permit.map(|permit| RetainedPhysicalCheckpoint {
            checkpoint,
            _permit: permit,
        })
    }

    pub(super) fn schedule_physical(
        self: &Arc<Self>,
        repository: &Repository,
        root: crab_metadata::capsule_protocol::RootSnapshot,
        retained: Option<RetainedPhysicalCheckpoint>,
    ) {
        let key = repository.config.name.clone();
        {
            let mut running = self
                .physical_repositories
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.cancellation.is_cancelled()
                || running.len() >= MAX_TRACKED_REFS
                || !running.insert(key.clone())
            {
                return;
            }
            self.active.fetch_add(1, Ordering::AcqRel);
        }
        let scheduler = Arc::clone(self);
        let repository = repository.clone();
        tokio::spawn(async move {
            let permit = tokio::select! {
                permit = Arc::clone(&scheduler.physical_permits).acquire_owned() => permit.ok(),
                () = scheduler.cancellation.cancelled() => None,
            };
            if let Some(_permit) = permit {
                let cancellation = scheduler.cancellation.child_token();
                let handle = tokio::runtime::Handle::current();
                // Checkpoint decoding, visibility folding and serialization can
                // take seconds. Keep that CPU work off request runtime workers,
                // and await its owner even when shutdown cancels publication.
                let started = Instant::now();
                let metrics = scheduler.metrics.clone();
                let result = tokio::task::spawn_blocking(move || {
                    handle.block_on(async {
                        let maximum_scratch_bytes = metrics.background_scratch_budget(
                            MAX_WARM_BRANCH_STATE_BYTES as u64,
                        )?;
                        let fences = GcWriterFences::acquire(&repository, &cancellation).await?;
                        let retained = retained.filter(|retained| {
                            root.record().root().checkpoint().is_some_and(|pointer| {
                                retained.checkpoint.matches_pointer(pointer).is_ok_and(|matches| matches)
                            })
                        });
                        let result = if let Some(retained) = retained {
                            let RetainedPhysicalCheckpoint { checkpoint, _permit } = retained;
                            tracing::info!(phase = "metadata_reuse", elapsed_ms = 0_u64, "S3 physical checkpoint reuses committed metadata");
                            crab_remote::checkpoint::repack_capsule_checkpoint_from_receipt_with_scratch_limit(
                                &repository.layout, root, checkpoint, MAX_CAPSULE_READ_BYTES,
                                maximum_scratch_bytes, &cancellation,
                            ).await.map_err(Error::from)
                        } else {
                            crab_remote::checkpoint::repack_capsule_checkpoint_from_root_with_scratch_limit(
                                &repository.layout, root, MAX_CAPSULE_READ_BYTES,
                                maximum_scratch_bytes, &cancellation,
                            ).await.map_err(Error::from)
                        };
                        fences.release().await;
                        if result.as_ref().is_ok_and(|work| work.published) {
                            repository.read_views.invalidate().await;
                        }
                        result
                    })
                })
                .await;
                match result {
                    Ok(Ok(work)) => tracing::info!(
                        published = work.published,
                        pack_bytes_read = work.pack_bytes_read,
                        pack_bytes_written = work.pack_bytes_written,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "S3 physical checkpoint maintenance finished"
                    ),
                    result if scheduler.cancellation.is_cancelled() => drop(result),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "S3 physical checkpoint maintenance failed")
                    }
                    Err(error) => {
                        tracing::warn!(%error, "S3 physical checkpoint maintenance worker failed")
                    }
                }
            } else {
                // Release queued metadata before announcing drained shutdown.
                drop(retained);
            }
            scheduler
                .physical_repositories
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            scheduler.active.fetch_sub(1, Ordering::AcqRel);
            scheduler.idle.notify_waiters();
        });
    }
}

pub(super) async fn revalidate_cached_branch(
    repository: &Repository,
    branch: &str,
    base: &crab_metadata::capsule_protocol::RootSnapshot,
    cached: Option<CachedBranchState>,
) -> Result<Option<CachedBranchState>> {
    let Some(mut cached) = cached else {
        return Ok(None);
    };
    let view = crab_read::capsule_protocol::open_view_from_root_with_layered_control(
        &repository.layout,
        base.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_CAPSULE_READ_BYTES,
            max_frontier_bytes: MAX_CAPSULE_READ_BYTES,
        },
    )
    .await?;
    if view.refs().get(branch) != cached.tip.map(|tip| tip.to_string()).as_ref() {
        return Ok(None);
    }
    cached.transaction = view.visible_ref_transactions().get(branch).cloned();
    cached.capsule_frontier = Some(CapsuleFrontier::from_view(&view, branch));
    // A physical checkpoint may replace pack sources. Logical tree entries
    // remain valid, but old physical delta anchors need fresh admission.
    cached.tree.forget_pack_provenance();
    Ok(Some(cached))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::tests::{capsule_fixture, coordinated_put, read};

    async fn checkpoint(
        repository: &Repository,
        cancel: &CancellationToken,
    ) -> crab_metadata::capsule_protocol::RootSnapshot {
        assert!(
            crab_remote::checkpoint::publish_capsule_checkpoint(
                &repository.layout,
                1,
                MAX_CAPSULE_READ_BYTES,
                cancel,
            )
            .await
            .unwrap()
            .published
        );
        crab_metadata::capsule_protocol::load_root(&repository.layout)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retained_checkpoint_shares_the_existing_manifest_budget() {
        let (repository, runtime, cancel) = capsule_fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            Default::default(),
            cancel.child_token(),
            Metrics::new().unwrap(),
        );
        coordinated_put(&coordinator, &repository, &cancel, "retained", b"data").await;
        coordinator.shutdown().await;
        let (root, checkpoint) = crab_remote::checkpoint::publish_capsule_checkpoint_with_receipt(
            &repository.layout,
            1,
            MAX_CAPSULE_READ_BYTES,
            &cancel,
        )
        .await
        .unwrap()
        .committed
        .unwrap();
        let bytes = checkpoint.retained_metadata_bytes().unwrap();
        let scheduler = Arc::new(CheckpointScheduler::new(
            cancel.child_token(),
            Metrics::new().unwrap(),
        ));
        let retained = scheduler.retain_checkpoint(checkpoint.clone()).unwrap();
        assert_eq!(
            scheduler.manifest_bytes.available_permits(),
            MAX_WARM_BRANCH_STATE_BYTES - bytes
        );
        drop(retained);
        assert_eq!(
            scheduler.manifest_bytes.available_permits(),
            MAX_WARM_BRANCH_STATE_BYTES
        );
        let _occupied = Arc::clone(&scheduler.manifest_bytes)
            .acquire_many_owned(MAX_WARM_BRANCH_STATE_BYTES as u32)
            .await
            .unwrap();
        assert!(scheduler.retain_checkpoint(checkpoint.clone()).is_none());
        drop(_occupied);
        let _blocked = Arc::clone(&scheduler.physical_permits)
            .acquire_owned()
            .await
            .unwrap();
        let retained = scheduler.retain_checkpoint(checkpoint).unwrap();
        scheduler.schedule_physical(&repository, root, Some(retained));
        assert_eq!(
            scheduler.manifest_bytes.available_permits(),
            MAX_WARM_BRANCH_STATE_BYTES - bytes
        );
        scheduler.shutdown().await;
        assert_eq!(
            scheduler.manifest_bytes.available_permits(),
            MAX_WARM_BRANCH_STATE_BYTES
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn physical_jobs_coalesce_bound_admission_and_drain_on_shutdown() {
        let (repository, runtime, cancel) = capsule_fixture().await;
        let root = crab_metadata::capsule_protocol::load_root(&repository.layout)
            .await
            .unwrap();
        let scheduler = Arc::new(CheckpointScheduler::new(cancel, Metrics::new().unwrap()));
        let _blocked = Arc::clone(&scheduler.physical_permits)
            .acquire_owned()
            .await
            .unwrap();
        scheduler.schedule_physical(&repository, root.clone(), None);
        scheduler.schedule_physical(&repository, root.clone(), None);
        assert_eq!(scheduler.active.load(Ordering::Acquire), 1);
        for index in 0..MAX_TRACKED_REFS {
            let mut other = repository.clone();
            other.config.name = format!("physical-job-{index}");
            scheduler.schedule_physical(&other, root.clone(), None);
        }
        assert_eq!(scheduler.active.load(Ordering::Acquire), MAX_TRACKED_REFS);
        tokio::time::timeout(Duration::from_secs(5), scheduler.shutdown())
            .await
            .unwrap();
        assert_eq!(scheduler.active.load(Ordering::Acquire), 0);
        assert!(scheduler.physical_repositories.lock().unwrap().is_empty());
        scheduler.schedule_physical(&repository, root, None);
        assert_eq!(scheduler.active.load(Ordering::Acquire), 0);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_physical_checkpoint_preserves_newer_writes_and_cold_reads() {
        let (repository, runtime, cancel) = capsule_fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            Default::default(),
            cancel.child_token(),
            Metrics::new().unwrap(),
        );
        for index in 0..64_u32 {
            coordinated_put(
                &coordinator,
                &repository,
                &cancel,
                &format!("object-{index}"),
                &index.to_le_bytes(),
            )
            .await;
        }
        let (root, before) = crab_remote::checkpoint::publish_capsule_checkpoint_with_receipt(
            &repository.layout,
            1,
            MAX_CAPSULE_READ_BYTES,
            &cancel,
        )
        .await
        .unwrap()
        .committed
        .unwrap();
        let retained = coordinator
            .checkpoints
            .retain_checkpoint(before.clone())
            .unwrap();
        coordinator
            .checkpoints
            .schedule_physical(&repository, root.clone(), Some(retained));
        for index in 64..104_u32 {
            coordinated_put(
                &coordinator,
                &repository,
                &cancel,
                &format!("object-{index}"),
                &index.to_le_bytes(),
            )
            .await;
        }
        coordinator.checkpoints.wait_idle().await;
        let after_root = crab_metadata::capsule_protocol::load_root(&repository.layout)
            .await
            .unwrap();
        let after = crab_metadata::capsule_protocol::load_layered_checkpoint(
            &repository.layout,
            after_root.record().root().checkpoint().unwrap(),
            MAX_CAPSULE_READ_BYTES,
        )
        .await
        .unwrap();
        assert_ne!(after_root.record().digest(), root.record().digest());
        assert_eq!(
            after_root.record().root().refs(),
            root.record().root().refs()
        );
        assert_eq!(
            after.cold_clone_object_set_digest(),
            before.cold_clone_object_set_digest()
        );
        assert!(after.pack_count().unwrap() < before.pack_count().unwrap());
        coordinator.shutdown().await;
        runtime.shutdown().await;
        let cold = Repository::new(repository.config.clone(), repository.store.clone()).unwrap();
        let cold_runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
        for index in 0..104_u32 {
            assert_eq!(
                read(
                    &cold,
                    Arc::clone(&cold_runtime),
                    &cancel,
                    &format!("object-{index}")
                )
                .await
                .as_ref(),
                index.to_le_bytes()
            );
        }
        cold_runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_physical_job_cannot_replace_a_newer_logical_checkpoint() {
        let (repository, runtime, cancel) = capsule_fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            Default::default(),
            cancel.child_token(),
            Metrics::new().unwrap(),
        );
        for index in 0..64_u32 {
            coordinated_put(
                &coordinator,
                &repository,
                &cancel,
                &format!("object-{index}"),
                &index.to_le_bytes(),
            )
            .await;
        }
        let old_root = checkpoint(&repository, &cancel).await;
        let blocked = Arc::clone(&coordinator.checkpoints.physical_permits)
            .acquire_owned()
            .await
            .unwrap();
        coordinator
            .checkpoints
            .schedule_physical(&repository, old_root, None);
        coordinated_put(&coordinator, &repository, &cancel, "newer", b"retained").await;
        let new_root = checkpoint(&repository, &cancel).await;
        drop(blocked);
        coordinator.checkpoints.wait_idle().await;
        let observed = crab_metadata::capsule_protocol::load_root(&repository.layout)
            .await
            .unwrap();
        assert_eq!(observed.record().digest(), new_root.record().digest());
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "newer").await,
            Bytes::from_static(b"retained")
        );
        coordinator.shutdown().await;
        runtime.shutdown().await;
    }
}
