//! Reopening and verifying an exact immutable root.
//!
//! Verification walks the root graph, authenticates each object against
//! its descriptor, and only then hands back a `VerifiedRoot` the replica
//! can restore from.

use super::*;

impl CellReplica {
    /// Reopens and verifies an exact immutable root and its metadata graph.
    ///
    /// A same-store authenticated metadata cache may avoid body reads, while
    /// origin HEADs check cached metadata presence. Use
    /// [`Self::reachable_objects`] to authenticate current remote bytes and
    /// inventory every dependency.
    pub async fn open_root(&self, root: &RootRef) -> Result<VerifiedRoot> {
        let graph = self.load_graph(root).await?;
        VerifiedRoot::from_graph(self.clone(), *root, &graph.document, graph.descriptors)
    }

    /// Verifies and returns the complete immutable dependency set for an exact root.
    ///
    /// Callers may use this bounded inventory for backup pinning and reachability
    /// collection. A missing or corrupt dependency fails the traversal closed.
    pub async fn reachable_objects(&self, root: &RootRef) -> Result<Vec<RootObjectRef>> {
        // Inventory must prove origin presence even for metadata uploaded here.
        let graph = self.load_graph_with_cache(root, false).await?;
        let extents = object_extents(&graph.descriptors)?;
        let verification = directory::Verification {
            layout: &self.layout,
            cell: &self.cell,
            incarnation: &self.incarnation,
            page_size: graph.document.page_size,
            database_pages: graph.document.database_pages,
            extents: &extents,
            host: &self.host,
            origin: crate::LtxReadOrigin::Cold,
        };
        let directory = directory::reachable_digests(
            verification,
            graph.document.directory_digest,
            graph.document.directory_height,
            graph.aggregate,
        )
        .await?;

        let mut objects = std::collections::BTreeSet::new();
        let mut streamed = std::collections::BTreeMap::new();
        objects.insert(RootObjectRef {
            digest: root.digest,
            kind: CellObjectKind::Root,
        });
        objects.extend(
            graph
                .document
                .segment_pages
                .iter()
                .map(|digest| RootObjectRef {
                    digest: *digest,
                    kind: CellObjectKind::Root,
                }),
        );
        for descriptor in &graph.descriptors {
            let body = RootObjectRef {
                digest: descriptor.object_digest(),
                kind: descriptor.object_kind(),
            };
            let body_limit = match body.kind {
                CellObjectKind::Bundle => self.limits.max_plan_bytes,
                CellObjectKind::Ltx => self.limits.max_file_bytes,
                _ => return Err(CrabError::LTXCorrupted),
            };
            let body_length =
                (body.kind == CellObjectKind::Ltx).then_some(descriptor.info.size_bytes);
            if streamed
                .insert(body, (body_limit, body_length))
                .is_some_and(|previous| previous != (body_limit, body_length))
            {
                return Err(CrabError::LTXCorrupted);
            }
            let index = RootObjectRef {
                digest: descriptor.index_digest,
                kind: CellObjectKind::Index,
            };
            if streamed
                .insert(
                    index,
                    (self.limits.max_plan_bytes, Some(descriptor.index_length)),
                )
                .is_some_and(|(_, length)| length != Some(descriptor.index_length))
            {
                return Err(CrabError::LTXCorrupted);
            }
        }
        for (object, (limit, length)) in &streamed {
            self.verify_remote_object(*object, *limit, *length).await?;
        }
        objects.extend(streamed.into_keys());
        objects.extend(directory.into_iter().map(|digest| RootObjectRef {
            digest,
            kind: CellObjectKind::Directory,
        }));
        Ok(objects.into_iter().collect())
    }

    async fn verify_remote_object(
        &self,
        object: RootObjectRef,
        max_bytes: u64,
        expected_bytes: Option<u64>,
    ) -> Result<()> {
        let path = self.layout.incarnation_object_path(
            &self.cell,
            &self.incarnation,
            &object.digest,
            object.kind,
        );
        let _permit = self.host.io_permit().await?;
        let request = self.layout.store().get_stream(&path, None).await;
        if request.is_err() {
            self.host
                .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, false, 0);
        }
        let (metadata, _, mut stream) = request?;
        if metadata.size > max_bytes || expected_bytes.is_some_and(|size| size != metadata.size) {
            self.host
                .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, true, 0);
            return Err(CrabError::LTXCorrupted);
        }
        let mut digest = blake3::Hasher::new();
        let mut read_bytes = 0usize;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => {
                    digest.update(&chunk);
                    read_bytes = read_bytes.saturating_add(chunk.len());
                }
                Ok(None) => break,
                Err(error) => {
                    self.host.observe_ltx_origin_request(
                        crate::LtxReadOrigin::Cold,
                        false,
                        read_bytes,
                    );
                    return Err(error.into());
                }
            }
        }
        self.host
            .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, true, read_bytes);
        if digest.finalize().as_bytes() != &object.digest {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    pub(super) async fn load_graph(&self, root: &RootRef) -> Result<LoadedGraph> {
        self.load_graph_with_cache(root, true).await
    }

    async fn load_graph_with_cache(&self, root: &RootRef, use_cache: bool) -> Result<LoadedGraph> {
        let started = self.host.now_monotonic();
        let result = self.load_graph_inner(root, use_cache).await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::RootOpen, started, result.is_ok());
        result
    }

    async fn load_graph_inner(&self, root: &RootRef, use_cache: bool) -> Result<LoadedGraph> {
        self.check_scope(root)?;
        let (bytes, cached_root) = self
            .read_object(&root.digest, CellObjectKind::Root, ROOT_BYTES, use_cache)
            .await?;
        if *blake3::hash(&bytes).as_bytes() != root.digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let document = decode_root(&bytes)?;
        if document.cell != self.cell
            || document.incarnation != self.incarnation
            || document.txid != root.position.txid
            || document.checksum != root.position.checksum
            || document.commit_sequence != root.commit_sequence
        {
            return Err(CrabError::InvalidState("Cell root reference mismatch"));
        }
        if document.segment_pages.is_empty() || document.segment_pages.len() > MAX_SEGMENT_PAGES {
            return Err(CrabError::LTXCorrupted);
        }
        let pages = stream::iter(
            document
                .segment_pages
                .iter()
                .copied()
                .map(|digest| async move {
                    let (bytes, cached) = self
                        .read_object(&digest, CellObjectKind::Root, SEGMENT_PAGE_BYTES, use_cache)
                        .await?;
                    if *blake3::hash(&bytes).as_bytes() != digest {
                        return Err(CrabError::ChecksumMismatch);
                    }
                    let page = decode_segment_page(&bytes)?;
                    if page.is_empty() || page.len() > SEGMENTS_PER_PAGE {
                        return Err(CrabError::LTXCorrupted);
                    }
                    Ok((page, cached.then_some((digest, bytes.len()))))
                }),
        )
        .buffered(OBJECT_FETCH_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        let mut cached_metadata = Vec::new();
        if cached_root {
            cached_metadata.push((root.digest, bytes.len()));
        }
        let mut descriptors = Vec::new();
        for (page, cached) in pages {
            descriptors.extend(page);
            cached_metadata.extend(cached);
        }
        // A cached predecessor cannot justify a new root if its metadata has
        // disappeared from origin. Check all cached objects in one bounded wave.
        self.verify_cached_metadata(&cached_metadata).await?;
        self.validate_chain(&descriptors, root.position)?;
        for descriptor in &descriptors {
            descriptor.validate_published(self.limits)?;
        }
        let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
        if document.page_size != endpoint.info.page_size
            || document.database_pages != endpoint.info.database_pages
            || document.schema == 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        let extents = object_extents(&descriptors)?;
        let directory_started = self.host.now_monotonic();
        let aggregate = directory::verify_root(
            directory::Verification {
                layout: &self.layout,
                cell: &self.cell,
                incarnation: &self.incarnation,
                page_size: document.page_size,
                database_pages: document.database_pages,
                extents: &extents,
                host: &self.host,
                origin: crate::LtxReadOrigin::Cold,
            },
            document.directory_digest,
            document.directory_height,
        )
        .await;
        self.host.observe_ltx_phase(
            crate::LtxPhase::Directory,
            directory_started,
            aggregate.is_ok(),
        );
        let aggregate = aggregate?;
        if aggregate.checksum != document.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(LoadedGraph {
            aggregate,
            document,
            descriptors,
        })
    }

    pub(super) fn validate_chain(
        &self,
        descriptors: &[SegmentDescriptor],
        target: Position,
    ) -> Result<()> {
        if descriptors.is_empty() || descriptors.len() > MAX_SEGMENTS.min(self.limits.max_segments)
        {
            return Err(CrabError::Limit(crate::LimitKind::CellRootSegments));
        }
        let mut previous = Position::default();
        let mut page_size = None;
        let mut total = 0u64;
        for descriptor in descriptors {
            descriptor.validate(self.limits)?;
            let info = &descriptor.info;
            total = total
                .checked_add(info.size_bytes)
                .and_then(|value| value.checked_add(descriptor.index_length))
                .ok_or(CrabError::Limit(crate::LimitKind::CellRootBytes))?;
            if total > self.limits.max_plan_bytes
                || previous.txid.checked_add(1) != Some(info.min_txid)
                || info.pre_checksum != previous.checksum
                || page_size.is_some_and(|value| value != info.page_size)
            {
                return Err(CrabError::LTXCorrupted);
            }
            previous = info.position();
            page_size = Some(info.page_size);
        }
        if previous != target {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    fn check_scope(&self, root: &RootRef) -> Result<()> {
        if root.cell != self.cell || root.incarnation != self.incarnation {
            return Err(CrabError::InvalidState(
                "root belongs to another Cell incarnation",
            ));
        }
        Ok(())
    }

    async fn read_object(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        max_bytes: u64,
        use_cache: bool,
    ) -> Result<(Vec<u8>, bool)> {
        // Hot roots use the same immutable-cache contract as directory nodes;
        // the inventory path bypasses it to detect missing remote objects.
        if use_cache
            && kind == CellObjectKind::Root
            && let Some(bytes) =
                cache::get(&self.layout, &self.cell, &self.incarnation, *digest, kind)?
        {
            if bytes.len() as u64 > max_bytes {
                return Err(CrabError::LTXCorrupted);
            }
            return Ok((bytes.to_vec(), true));
        }
        let _permit = self.host.io_permit().await?;
        let path = self
            .layout
            .incarnation_object_path(&self.cell, &self.incarnation, digest, kind);
        let result = self
            .layout
            .store()
            .get_with_etag_bounded(&path, max_bytes)
            .await;
        self.host.observe_ltx_origin_request(
            crate::LtxReadOrigin::Cold,
            result.is_ok(),
            result.as_ref().map_or(0, |(bytes, _)| bytes.len()),
        );
        let (bytes, _) = result?;
        if kind == CellObjectKind::Root {
            if *blake3::hash(&bytes).as_bytes() != *digest {
                return Err(CrabError::ChecksumMismatch);
            }
            cache::insert(
                &self.layout,
                &self.cell,
                &self.incarnation,
                *digest,
                kind,
                bytes.to_vec().into(),
            )?;
        }
        Ok((bytes.to_vec(), false))
    }

    async fn verify_cached_metadata(&self, objects: &[([u8; 32], usize)]) -> Result<()> {
        stream::iter(objects.iter().copied().map(|(digest, size)| async move {
            let _permit = self.host.io_permit().await?;
            let path = self.layout.incarnation_object_path(
                &self.cell,
                &self.incarnation,
                &digest,
                CellObjectKind::Root,
            );
            let result = self.layout.store().head(&path).await;
            self.host
                .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, result.is_ok(), 0);
            if result?.size != size as u64 {
                return Err(CrabError::LTXCorrupted);
            }
            Ok(())
        }))
        .buffer_unordered(OBJECT_FETCH_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        Ok(())
    }
}

impl VerifiedRoot {
    pub(super) fn from_graph(
        replica: CellReplica,
        root: RootRef,
        document: &RootDocument,
        descriptors: Vec<SegmentDescriptor>,
    ) -> Result<Self> {
        let extents = object_extents(&descriptors)?;
        Ok(Self {
            root,
            page_size: document.page_size,
            database_pages: document.database_pages,
            schema: document.schema,
            segment_count: descriptors.len(),
            directory_height: document.directory_height,
            pages: CellPagedDatabase {
                replica,
                directory_digest: document.directory_digest,
                directory_height: document.directory_height,
                extents: Arc::new(extents),
                page_size: document.page_size,
                database_pages: document.database_pages,
                position: root.position,
            },
        })
    }
}
