use crab_storage::cdn::PayloadObservation;
use futures_util::future::try_join_all;

use super::edge_entry::edge_entry_can_restart;
use super::*;

enum EdgeWindow {
    Origin,
    Bytes(Bytes, Option<PayloadObservation>),
}

pub(super) struct EdgePackedBatch<'a> {
    store: &'a Store,
    edge_oids: HashSet<gix_hash::ObjectId>,
    observations: Vec<PayloadObservation>,
}

impl RemoteGitReader {
    pub(super) async fn read_verified_packed_batch(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: &[gix_hash::ObjectId],
        locators: &[GitObjectLocator],
        concurrency: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObject>> {
        let plan = self.plan_packed_ranges(requested, locators)?;
        let cancel = cancellation.child_token();
        let _cancel_on_exit = cancel.clone().drop_guard();
        let from_origin = async {
            let entries = self
                .read_packed_ranges_at_origin(&plan, concurrency, budget, &cancel)
                .await?;
            self.materialize_packed_entries(
                session,
                entries,
                requested,
                concurrency,
                budget,
                &cancel,
            )
            .await
        };
        let Some(anchor) = plan.ranges.iter().find_map(|range| {
            let (path, size) = self.authenticated_range_source(range)?;
            let length = range.end.checked_sub(range.start)?;
            self.store
                .cdn_attempt(path)
                .filter(|edge| edge.admits(size, Some(length)))
        }) else {
            return from_origin.await;
        };
        let store = self
            .store
            .clone()
            .with_read_admission(budget.read_admission(cancel.clone()));

        let (objects, observations) = anchor
            .within_deadline(async {
                let mut batch = EdgePackedBatch {
                    store: &store,
                    edge_oids: HashSet::new(),
                    observations: Vec::new(),
                };
                if let Some(entries) = self
                    .read_packed_ranges_with_edge(&mut batch, &plan, concurrency, budget, &cancel)
                    .await?
                {
                    let result = self
                        .materialize_packed_entries_with_edge(
                            session,
                            entries,
                            requested,
                            concurrency,
                            budget,
                            &cancel,
                            Some(&mut batch),
                        )
                        .await;
                    match result {
                        Err(error)
                            if !batch.edge_oids.is_empty()
                                && edge_entry_can_restart(
                                    &error,
                                    |oid| batch.edge_oids.contains(oid),
                                    false,
                                ) =>
                        {
                            tracing::warn!(
                                "CDN Git batch failed verification; reading from selected origin"
                            );
                        }
                        Ok(Some(objects)) => return Ok((objects, batch.observations)),
                        Ok(None) => {}
                        Err(error) => return Err(error),
                    }
                }
                from_origin.await.map(|objects| (objects, Vec::new()))
            })
            .await?;
        for observation in observations {
            observation.record_verified();
        }
        Ok(objects)
    }

    fn authenticated_range_source<'a>(
        &self,
        range: &'a CoalescedRange,
    ) -> Option<(&'a ObjectPath, u64)> {
        let CoalescedRangeSource::Object(path) = &range.source else {
            return None;
        };
        let mut sizes = range.entries.iter().map(|entry| {
            self.pack_source(&entry.locator.pack_id)?
                .authenticated_object_size()
        });
        let size = sizes.next().flatten()?;
        (range.end <= size && sizes.all(|other| other == Some(size))).then_some((path, size))
    }

    pub(super) async fn read_packed_ranges_with_edge(
        &self,
        batch: &mut EdgePackedBatch<'_>,
        plan: &PackedRangePlan,
        concurrency: usize,
        budget: &OperationBudget,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<RemoteGitPackedEntry>>> {
        let mut entries = Vec::with_capacity(plan.selected.len());
        for wave in plan.ranges.chunks(concurrency.max(1)) {
            // Resolve the in-flight edge outcomes before parsing or restarting.
            // A recoverable response cannot hide a sibling's terminal denial.
            let windows = try_join_all(
                (0..wave.len())
                    .map(|index| self.read_edge_window(batch.store, &wave[index], budget, cancel)),
            )
            .await?;
            let Some(windows) = windows.into_iter().collect::<Option<Vec<_>>>() else {
                return Ok(None);
            };
            let parsed = try_join_all(windows.into_iter().enumerate().map(
                |(index, window)| async move {
                    let range = &wave[index];
                    let parsed = |bytes| {
                        self.parse_packed_range(range, bytes, &plan.selected, budget, cancel)
                    };
                    match window {
                        EdgeWindow::Origin => {
                            let data = self.read_coalesced_range(range, budget, cancel).await?;
                            Ok(Some((parsed(data).await?, false, None)))
                        }
                        EdgeWindow::Bytes(data, observation) => match parsed(data).await {
                            Err(error)
                                if edge_entry_can_restart(
                                    &error,
                                    |oid| range.entries.iter().any(|entry| entry.oid == *oid),
                                    false,
                                ) =>
                            {
                                Ok(None)
                            }
                            result => result.map(|entries| Some((entries, true, observation))),
                        },
                    }
                },
            ))
            .await?;
            for group in parsed {
                let Some((group, from_edge, observation)) = group else {
                    return Ok(None);
                };
                if from_edge && !group.is_empty() {
                    batch.edge_oids.extend(group.iter().map(|entry| entry.oid));
                    batch.observations.extend(observation);
                }
                entries.extend(group);
            }
        }
        Ok(Some(entries))
    }

    async fn read_edge_window(
        &self,
        store: &Store,
        range: &CoalescedRange,
        budget: &OperationBudget,
        cancel: &CancellationToken,
    ) -> Result<Option<EdgeWindow>> {
        let Some((path, size)) = self.authenticated_range_source(range) else {
            return Ok(Some(EdgeWindow::Origin));
        };
        let length = range.end.checked_sub(range.start).ok_or(Error::Corrupt {
            stage: CorruptionStage::PackEntry,
        })?;
        let Some(attempt) = store
            .cdn_attempt(path)
            .filter(|edge| edge.admits(size, Some(length)))
        else {
            return Ok(Some(EdgeWindow::Origin));
        };
        check_limit(
            "fetched bytes",
            length,
            budget.remaining(BudgetDimension::FetchedBytes).await,
        )?;
        let memory = self
            .runtime
            .reserve_bytes(usize_limit(length, "resident read bytes")?)
            .await?;
        let permit = self.runtime.origin_permit(cancel).await?;
        let bytes = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::Cancelled),
            result = attempt.get(size, Some(range.start..range.end)) => result?,
        };
        drop(permit);
        bytes
            .map(|bytes| {
                observe_storage_read("range_get", bytes.len() as u64);
                Ok(EdgeWindow::Bytes(
                    memory.bind(bytes)?,
                    attempt.payload_observation(),
                ))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crab_storage::StoreLayout;
    use crab_storage::cdn::{CdnAccess, CdnRoute, CdnTransport};
    use object_store::memory::InMemory;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn packed_batch_denial_wins_over_a_recoverable_sibling() {
        let target = [1; 32];
        let origin = Store::new(Arc::new(InMemory::new())).with_target_identity(target);
        let layout = StoreLayout::new(origin, "repository".to_owned());
        let mut inventory = Vec::new();
        let mut requested = Vec::new();
        let mut locators = HashMap::new();
        let mut sources = HashMap::new();
        let sidecar = |offset| RemoteGitSidecarRange {
            offset,
            length: 1,
            blake3: "a".repeat(64),
        };
        for (digit, oid_byte) in [('1', 1), ('2', 2)] {
            let hash = digit.to_string().repeat(64);
            let pack_id = MerkleHash::from_hex(&hash).unwrap();
            let path = layout.capsule_pack_layer_path(&hash);
            let oid = [oid_byte; 20];
            requested.push(gix_hash::ObjectId::from(oid));
            inventory.push(GitPackInventoryEntry {
                pack_id,
                pack_size: 45,
                object_count: 1,
            });
            locators.insert(
                oid,
                GitObjectLocator {
                    ordinal: 0,
                    pack_id,
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 10,
                        crc32: 0,
                    },
                    metadata: Default::default(),
                },
            );
            sources.insert(
                pack_id,
                RemoteGitPackSource::embedded_lazy_index(path, 1, 45, 50, sidecar(46), sidecar(47))
                    .unwrap(),
            );
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
        let edge = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(socket.read_line(&mut request).await.unwrap(), 0);
                }
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("range: bytes=13-22\r\n")
                );
                requests.push(socket);
            }
            requests[0].write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            requests[1]
                .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let store = layout
            .store()
            .clone()
            .with_cdn_route(CdnRoute::new(
                target,
                layout.repo_prefix().to_owned(),
                None,
                CdnAccess::Public,
                transport,
                Duration::from_secs(2),
            ))
            .unwrap();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let reader = Arc::new(
            RemoteGitReader::from_pinned_with_preferred_pack_indexes(
                store,
                layout.repo_prefix(),
                inventory,
                SnapshotLookupSources::default()
                    .with_inline_locators(locators)
                    .with_pack_sources(sources),
                ReaderLimits::default(),
                runtime.clone(),
                RepositoryIdentity::new("provider", "repository", 1).unwrap(),
                1,
            )
            .unwrap(),
        );
        let budget = OperationBudget::new(crate::OperationLimits::default(), runtime.clone());
        let error = reader
            .read_many_with_session(
                &GitObjectLocatorSession::without_catalog(),
                &requested,
                2,
                &budget,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            crab_storage::store::cdn_read_rejection(&error),
            Some(crab_storage::StorageError::Forbidden { .. })
        ));
        assert_eq!(
            budget
                .usage()
                .await
                .amount(BudgetDimension::StorageRequests),
            2
        );
        tokio::time::timeout(Duration::from_secs(2), edge)
            .await
            .unwrap()
            .unwrap();
        runtime.shutdown().await;
    }
}
