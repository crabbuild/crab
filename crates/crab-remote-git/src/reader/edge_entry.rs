use super::*;

impl RemoteGitReader {
    pub(super) async fn read_resolved_locator(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        max_object_bytes: u64,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let origin = || {
            self.read_origin_locator(
                session,
                requested,
                locator,
                max_object_bytes,
                budget,
                cancellation,
            )
        };
        let Some(source_size) = self
            .pack_source(&locator.pack_id)
            .and_then(RemoteGitPackSource::authenticated_object_size)
        else {
            return origin().await;
        };
        let end = self.checked_packed_entry_end(locator, budget).await?;
        let Some((path, range)) =
            self.pack_path_range(&locator.pack_id, locator.location.pack_offset, end)?
        else {
            return origin().await;
        };
        let attempt_cancel = cancellation.child_token();
        let _cancel_on_exit = attempt_cancel.clone().drop_guard();
        let store = self
            .store
            .clone()
            .with_read_admission(budget.read_admission(attempt_cancel.clone()));
        let Some(attempt) = store.cdn_attempt(&path) else {
            let object = origin().await?;
            if object.kind == gix_object::Kind::Blob {
                store.observe_verified_unlisted_git_blob(&path, requested.as_bytes());
            }
            return Ok(object);
        };
        let (object, from_edge) = attempt
            .within_deadline(async {
                let memory = self
                    .runtime
                    .reserve_bytes(usize_limit(
                        locator.location.entry_len,
                        "resident read bytes",
                    )?)
                    .await?;
                let permit = self.runtime.origin_permit(&attempt_cancel).await?;
                let bytes = tokio::select! {
                    biased;
                    () = attempt_cancel.cancelled() => return Err(Error::Cancelled),
                    result = attempt.get(source_size, Some(range)) => result?,
                };
                drop(permit);
                if let Some(bytes) = bytes {
                    // The packed header and CRC cannot authenticate a delta.
                    // Resolve its base through the pinned reader and verify the final OID.
                    observe_storage_read("range_get", bytes.len() as u64);
                    let replan = Arc::new(AtomicBool::new(false));
                    let result = self
                        .verify_edge_entry(
                            session,
                            requested,
                            locator,
                            memory.bind(bytes)?,
                            DeltaReadState {
                                max_object_bytes,
                                remaining_depth: self.limits.max_delta_depth,
                                ancestors: HashSet::new(),
                                replan: replan.clone(),
                                planned_bases: None,
                            },
                            budget,
                            &attempt_cancel,
                        )
                        .await;
                    match result {
                        Ok(object) => return Ok((object, true)),
                        Err(error)
                            if edge_entry_can_restart(
                                &error,
                                |oid| *oid == requested,
                                replan.load(Ordering::Acquire),
                            ) =>
                        {
                            tracing::warn!(
                                "CDN Git entry failed verification; reading from selected origin"
                            );
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    drop(memory);
                }
                self.read_origin_locator(
                    session,
                    requested,
                    locator,
                    max_object_bytes,
                    budget,
                    &attempt_cancel,
                )
                .await
                .map(|object| (object, false))
            })
            .await?;
        if from_edge && let Some(observation) = attempt.payload_observation() {
            if object.kind == gix_object::Kind::Blob {
                observation.record_verified_git_blob(requested.as_bytes());
            } else {
                observation.record_verified();
            }
        }
        Ok(object)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the verifier owns one located entry and its delta state"
    )]
    async fn verify_edge_entry(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        bytes: Bytes,
        delta: DeltaReadState,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let inflated_bytes =
            self.checked_inflated_size(requested, locator, &bytes, delta.max_object_bytes)?;
        budget
            .charge(BudgetDimension::InflatedBytes, inflated_bytes)
            .await?;
        let packed = self
            .inflate_packed_entry(
                requested,
                locator.location.pack_offset,
                bytes,
                delta.max_object_bytes,
                cancellation,
            )
            .await?;
        self.read_from_locator(
            session,
            requested,
            locator,
            Some(packed),
            delta,
            budget,
            cancellation,
        )
        .await
    }
}

pub(super) fn edge_entry_can_restart(
    error: &Error,
    from_edge: impl Fn(&gix_hash::ObjectId) -> bool,
    replan: bool,
) -> bool {
    match error {
        Error::PackedEntryCrcMismatch { oid }
        | Error::PackEntry { oid, .. }
        | Error::Inflate { oid, .. }
        | Error::InvalidInflatedEntry { oid, .. }
        | Error::InvalidDelta { oid, .. }
        | Error::ObjectIdMismatch { oid, .. } => from_edge(oid),
        Error::Corrupt {
            stage: CorruptionStage::PackEntry | CorruptionStage::Delta,
        }
        | Error::DeltaBaseNotFound { .. }
        | Error::ObjectNotFound { .. } => true,
        Error::LimitExceeded {
            limit: "delta depth",
            ..
        } => replan,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::StoreLayout;
    use object_store::memory::InMemory;
    use std::io::Write as _;

    #[tokio::test]
    async fn layered_git_reads_verify_final_oid_before_accepting_edge_bytes() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        use crab_storage::cdn::{CdnAccess, CdnConsumer, CdnProfile, CdnRoute, CdnTransport};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tracing_subscriber::{Layer, layer::Context, prelude::*};

        struct VerifiedPayloads(Arc<AtomicUsize>);

        impl<S: tracing::Subscriber> Layer<S> for VerifiedPayloads {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                let metadata = event.metadata();
                if metadata.target() == "crab_storage::cdn::transfer"
                    && metadata.fields().field("status").is_none()
                {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let hash_blob = |data: &[u8]| {
            gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, data).unwrap()
        };
        let base = b"hello world";
        let expected = b"hello worldQJ\xa8\x8e\x06Y\x07H";
        let base_oid = hash_blob(base);
        let oid = hash_blob(expected);
        let mut base_entry = Vec::new();
        Header::Blob
            .write_to(base.len() as u64, &mut base_entry)
            .unwrap();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(base).unwrap();
        base_entry.extend(encoder.finish().unwrap());

        // Both zlib streams have the same packed length and CRC, but yield different Git blobs.
        let encode_delta = |compressed: &[u8]| {
            let mut entry = Vec::new();
            Header::RefDelta { base_id: base_oid }
                .write_to(13, &mut entry)
                .unwrap();
            entry.extend(compressed);
            entry
        };
        let valid = encode_delta(
            b"\x78\x9c\xe3\x16\x9e\xc0\xcd\x11\xe8\xb5\xa2\x8f\x2d\x92\xdd\x03\x00\x15\x23\x03\x41",
        );
        let invalid = encode_delta(
            b"\x78\x9c\xe3\x16\x9e\xc0\xcd\x61\x74\x6c\xda\xc7\x59\xce\xf1\x9e\x00\x1b\xd1\x04\xc6",
        );
        let crc = gix_features::hash::crc32(&valid);
        assert_eq!(valid.len(), invalid.len());
        assert_eq!(crc, gix_features::hash::crc32(&invalid));
        let mut pack = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        let base_offset = pack.len() as u64;
        pack.extend(&base_entry);
        let entry_offset = pack.len() as u64;
        pack.extend(&valid);
        pack.extend_from_slice(&Sha1::digest(&pack));
        let pack_id = MerkleHash::from_hex(&blake3::hash(&pack).to_hex()).unwrap();
        let pack_size = pack.len() as u64;
        let mut layer = b"layer".to_vec();
        let object_offset = layer.len() as u64;
        layer.extend(pack);
        let index_offset = layer.len() as u64;
        layer.extend(b"ir");
        let size = layer.len() as u64;
        let target = [1; 32];
        let origin = Store::new(Arc::new(InMemory::new())).with_target_identity(target);
        let layout = StoreLayout::new(origin, "repository".to_owned());
        let path = layout.capsule_pack_layer_path(&blake3::hash(&layer).to_hex());
        layout
            .store()
            .create_strict(&path, Bytes::from(layer))
            .await
            .unwrap();
        let sidecar = |offset, data: &[u8]| RemoteGitSidecarRange {
            offset,
            length: 1,
            blake3: blake3::hash(data).to_hex().to_string(),
        };
        let source = RemoteGitPackSource::embedded_lazy_index(
            path.clone(),
            object_offset,
            pack_size,
            size,
            sidecar(index_offset, b"i"),
            sidecar(index_offset + 1, b"r"),
        )
        .unwrap();
        let locate = |offset, bytes: &[u8]| GitObjectLocator {
            ordinal: 0,
            pack_id,
            location: GitObjectLocation {
                pack_offset: offset,
                entry_len: bytes.len() as u64,
                crc32: gix_features::hash::crc32(bytes),
            },
            metadata: Default::default(),
        };
        let end = object_offset + entry_offset + valid.len() as u64 - 1;
        enum Read {
            Point,
            Coalesced,
            Dependency,
        }
        for (read, edge_bytes, expected_requests, budget_exhausted) in [
            (Read::Point, Some(&valid), 2, false),
            (Read::Point, Some(&invalid), 3, false),
            (Read::Point, None, 1, false),
            (Read::Point, Some(&valid), 1, true),
            (Read::Coalesced, Some(&valid), 1, false),
            (Read::Coalesced, Some(&invalid), 2, false),
            (Read::Coalesced, None, 1, false),
            (Read::Coalesced, Some(&valid), 1, true),
            (Read::Dependency, Some(&valid), 2, false),
        ] {
            let coalesced = matches!(read, Read::Coalesced);
            let expected_observations = if budget_exhausted || edge_bytes != Some(&valid) {
                0
            } else if matches!(read, Read::Dependency) {
                2
            } else {
                1
            };
            let observations = Arc::new(AtomicUsize::new(0));
            let dispatch = tracing::Dispatch::new(
                tracing_subscriber::registry().with(VerifiedPayloads(observations.clone())),
            );
            let _trace = tracing::dispatcher::set_default(&dispatch);
            let start = object_offset + if coalesced { base_offset } else { entry_offset };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
            let request_key = path.clone();
            let denied = edge_bytes.is_none();
            let response = if let Some(bytes) = edge_bytes {
                let body = if coalesced {
                    [base_entry.as_slice(), bytes].concat()
                } else {
                    bytes.clone()
                };
                let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\n\r\n",
                    body.len(),
                );
                [headers.as_bytes(), body.as_slice()].concat()
            } else {
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_vec()
            };
            let mut responses = vec![(start, end, response)];
            if matches!(read, Read::Dependency) {
                let start = object_offset + base_offset;
                let end = start + base_entry.len() as u64 - 1;
                let headers = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {}\r\n\r\n",
                    base_entry.len(),
                );
                responses.push((start, end, [headers.as_bytes(), &base_entry].concat()));
            }
            let edge = tokio::spawn(async move {
                for (start, end, response) in responses {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    while !request.ends_with("\r\n\r\n") {
                        assert_ne!(socket.read_line(&mut request).await.unwrap(), 0);
                    }
                    assert!(request.starts_with(&format!("GET /{request_key} HTTP/1.1\r\n")));
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains(&format!("range: bytes={start}-{end}\r\n"))
                    );
                    socket.write_all(&response).await.unwrap();
                }
            });
            let store = layout
                .store()
                .clone()
                .with_cdn_route(
                    CdnRoute::new(
                        target,
                        layout.repo_prefix().to_owned(),
                        None,
                        CdnAccess::Public,
                        transport,
                        Duration::from_secs(2),
                    )
                    .for_consumer(CdnConsumer::Cli, CdnProfile::Direct),
                )
                .unwrap();
            // Keep the verified base cache while making origin ranges exact.
            // A speculative pack window could otherwise satisfy the fallback
            // without a distinct on-wire request for the corrupt edge entry.
            let runtime = Arc::new(
                RemoteGitRuntime::new(
                    crate::RuntimeOptions {
                        max_object_cache_bytes: crate::runtime::PACK_WINDOW_BYTES as usize - 1,
                        ..Default::default()
                    },
                    Arc::new(crate::NoopMetrics),
                )
                .unwrap(),
            );
            let reader = Arc::new(
                RemoteGitReader::from_pinned_with_preferred_pack_indexes(
                    store,
                    layout.repo_prefix(),
                    [GitPackInventoryEntry {
                        pack_id,
                        pack_size,
                        object_count: 2,
                    }],
                    SnapshotLookupSources::default()
                        .with_inline_locators(HashMap::from([
                            (
                                base_oid.as_bytes().try_into().unwrap(),
                                locate(base_offset, &base_entry),
                            ),
                            (
                                oid.as_bytes().try_into().unwrap(),
                                locate(entry_offset, &valid),
                            ),
                        ]))
                        .with_pack_sources(HashMap::from([(pack_id, source.clone())])),
                    ReaderLimits::default(),
                    runtime.clone(),
                    RepositoryIdentity::new("provider", "repository", 1).unwrap(),
                    1,
                )
                .unwrap(),
            );
            let mut limits = crate::OperationLimits::default();
            if budget_exhausted {
                limits.max_inflated_bytes = 1;
            }
            let budget = OperationBudget::new(limits, runtime.clone());
            let session = GitObjectLocatorSession::without_catalog();
            let cancel = CancellationToken::new();
            let result = match read {
                Read::Coalesced => {
                    reader
                        .read_many_with_session(
                            &session,
                            &[oid, base_oid, oid],
                            2,
                            &budget,
                            &cancel,
                        )
                        .await
                }
                Read::Dependency => {
                    reader
                        .read_many_with_session(&session, &[oid], 2, &budget, &cancel)
                        .await
                }
                Read::Point => reader
                    .read_with_session(&session, oid, &budget, &cancel)
                    .await
                    .map(|result| vec![result]),
            };
            assert_eq!(observations.load(Ordering::Relaxed), expected_observations);
            if denied {
                assert!(crab_storage::store::cdn_read_rejection(&result.unwrap_err()).is_some());
            } else if budget_exhausted {
                assert!(matches!(
                    result,
                    Err(Error::LimitExceeded {
                        limit: "inflated bytes",
                        ..
                    })
                ));
            } else {
                let contents = result.unwrap();
                let payloads = contents
                    .iter()
                    .map(|object| object.data.as_ref())
                    .collect::<Vec<_>>();
                let wanted = if coalesced {
                    vec![expected.as_slice(), base.as_slice(), expected.as_slice()]
                } else {
                    vec![expected.as_slice()]
                };
                assert_eq!(payloads, wanted);
                if matches!(read, Read::Point) && edge_bytes == Some(&valid) {
                    let cached = reader
                        .read_with_session(&session, oid, &budget, &cancel)
                        .await
                        .unwrap();
                    assert_eq!(cached.data.as_ref(), expected);
                    assert_eq!(
                        observations.load(Ordering::Relaxed),
                        expected_observations + 1
                    );
                }
            }
            let usage = budget.usage().await;
            assert_eq!(
                usage.amount(BudgetDimension::StorageRequests),
                expected_requests
            );
            if coalesced && !denied && !budget_exhausted {
                let batch_inflated = base.len() as u64 + 13 + expected.len() as u64;
                assert_eq!(
                    usage.amount(BudgetDimension::InflatedBytes),
                    batch_inflated * expected_requests,
                );
            }
            tokio::time::timeout(Duration::from_secs(2), edge)
                .await
                .unwrap()
                .unwrap();
            runtime.shutdown().await;
        }
    }
}
