//! Preparing an immutable root and its follower-visible inputs.
//!
//! Every entry point here verifies the base it continues, admits the
//! segments, uploads the objects a root needs, and finishes only once the
//! root document matches the admitted frames.

use super::*;

impl CellReplica {
    /// Verifies and uploads a new immutable root without changing authority.
    pub async fn prepare(
        &self,
        base: Option<&RootRef>,
        cuts: &CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        let mut replica = self.clone();
        replica.host = self.host.for_dirty().await?;
        replica
            .prepare_captured(base, cuts, commit_sequence, schema)
            .await
    }

    async fn prepare_captured(
        &self,
        base: Option<&RootRef>,
        cuts: &CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        self.validate_metadata(commit_sequence, schema)?;
        if cuts.segments.is_empty() {
            return Err(CrabError::InvalidState("empty Cell append"));
        }
        let captured_bytes = cuts.segments.iter().try_fold(0_u64, |total, segment| {
            // A full database image may legitimately exceed the incremental
            // bound; the per-representation check below rejects an oversized
            // delta after its index proves the coverage.
            if segment.info().size_bytes > self.limits.max_file_bytes {
                return Err(CrabError::Limit(crate::LimitKind::CapturedCellLtxBytes));
            }
            total
                .checked_add(segment.info().size_bytes)
                .ok_or(CrabError::Limit(crate::LimitKind::CapturedCellLtxBytes))
        })?;
        if captured_bytes > self.limits.max_plan_bytes {
            return Err(CrabError::Limit(crate::LimitKind::CapturedCellLtxBytes));
        }
        let load_base = async {
            match base {
                Some(root) => self.load_graph(root).await.map(Some),
                None => Ok(None),
            }
        };
        // Local captures and the immutable predecessor cannot affect each
        // other; chain validation still waits for both exact inputs.
        let (base_graph, inputs) =
            futures_util::future::try_join(load_base, self.prepare_captured_inputs(&cuts.segments))
                .await?;
        self.validate_append_sequence(&base_graph, commit_sequence)?;

        // Admit the complete prospective chain from trusted capture metadata
        // before reading local bodies or starting immutable uploads.
        let mut descriptors = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        descriptors.extend(
            cuts.segments
                .iter()
                .map(|segment| SegmentDescriptor::native(segment.info().clone(), [0; 32], 0)),
        );
        self.validate_chain(&descriptors, cuts.position)?;

        // Keep each exact capture handle open through verification and upload.
        // A path replacement cannot redirect retries, while the inspected LTX
        // digest still rejects in-place mutation before authority may publish.
        self.prepare_append(
            base,
            base_graph,
            inputs,
            cuts.position,
            commit_sequence,
            schema,
            None,
        )
        .await
    }

    async fn prepare_captured_inputs(
        &self,
        segments: &[crate::LocalSegment],
    ) -> Result<Vec<AppendInput>> {
        stream::iter(segments.iter().cloned().map(|segment| async move {
            let source = segment.path().to_owned();
            let info = segment.info().clone();
            let source = upload::PinnedCapture::open(&self.host, source, info.size_bytes).await?;
            let index = match segment.captured_index() {
                Some(index) => index,
                None => Bytes::from(
                    upload::inspect_segment_source(self, Arc::clone(&source), &info).await?,
                ),
            };
            self.admit_segment_representation(&info, index.len())?;
            Ok(AppendInput {
                info,
                location: BodyLocation::Native,
                index,
                body: AppendBody::Native(source),
            })
        }))
        // Preserve descriptor order while overlapping independent file jobs.
        // Host job permits remain the shared process-wide admission boundary.
        .buffered(SEGMENT_TRANSFER_CONCURRENCY)
        .try_collect()
        .await
    }

    /// Admits one captured segment's representation against the publication bounds.
    ///
    /// A segment larger than the incremental bound must be a full database
    /// image: its encoded index has to cover every page the commit published.
    /// The capture writer already escalates an oversized delta to that
    /// representation, so a large partial index is a corrupt or foreign cut.
    fn admit_segment_representation(
        &self,
        info: &crate::SegmentInfo,
        index_bytes: usize,
    ) -> Result<()> {
        if info.size_bytes <= self.limits.max_capture_bytes {
            return Ok(());
        }
        let lock = crate::ltx::lock_pgno(info.page_size);
        let expected_pages =
            u64::from(info.database_pages) - u64::from(lock <= info.database_pages);
        if (index_bytes / crate::paged::ENTRY_BYTES) as u64 != expected_pages {
            return Err(CrabError::Limit(crate::LimitKind::CapturedCellLtxBytes));
        }
        Ok(())
    }

    /// Verifies selected Cell rows from a shared bundle and prepares one root append.
    ///
    /// Bundle row identity is routing metadata, not authorization. Only rows using
    /// the canonical Cell/incarnation identity are selected, and their complete LTX
    /// chain is independently verified before the immutable bundle is retained.
    pub async fn prepare_bundle(
        &self,
        base: Option<&RootRef>,
        bundle: &crate::bundle::Bundle,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        let mut replica = self.clone();
        replica.host = self.host.for_dirty().await?;
        replica
            .prepare_bundle_admitted(base, bundle, commit_sequence, schema)
            .await
    }

    /// Prepares the exact successor pinned by a recovered node-log overlay.
    ///
    /// Recovery policy and ownership remain caller-owned. This method accepts
    /// only this replica's Cell/incarnation rows, requires the declared final
    /// position to match the bundle, and reuses normal root preparation.
    pub async fn prepare_recovered_overlay(
        &self,
        overlay: &RecoveryOverlay,
        schema: u32,
    ) -> Result<PreparedRoot> {
        if overlay.predecessor.cell != self.cell
            || overlay.predecessor.incarnation != self.incarnation
            || overlay.final_commit_sequence <= overlay.predecessor.commit_sequence
        {
            return Err(CrabError::InvalidState("recovery overlay scope"));
        }
        let (repository, epoch) = crate::bundle::cell_identity(&self.cell, &self.incarnation);
        let final_position = overlay
            .bundle
            .rows()
            .iter()
            .rfind(|row| row.repository == repository && row.epoch == epoch)
            .map(|row| row.info.position())
            .ok_or(CrabError::TxNotAvailable)?;
        if final_position != overlay.final_position {
            return Err(CrabError::ChecksumMismatch);
        }
        let prepared = self
            .prepare_bundle(
                Some(&overlay.predecessor),
                &overlay.bundle,
                overlay.final_commit_sequence,
                schema,
            )
            .await?;
        if prepared.root().position != overlay.final_position {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(prepared)
    }

    async fn prepare_bundle_admitted(
        &self,
        base: Option<&RootRef>,
        bundle: &crate::bundle::Bundle,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        self.validate_metadata(commit_sequence, schema)?;
        if bundle.len() > self.limits.max_plan_bytes {
            return Err(CrabError::Limit(crate::LimitKind::CellBundleBytes));
        }
        let base_graph = match base {
            Some(root) => Some(self.load_graph(root).await?),
            None => None,
        };
        self.validate_append_sequence(&base_graph, commit_sequence)?;

        let (repository, epoch) = crate::bundle::cell_identity(&self.cell, &self.incarnation);
        let bundle_digest = bundle.digest();
        let mut inputs = Vec::new();
        let mut selected_bytes = 0_u64;
        let mut prospective = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        for (index, row) in bundle.rows().iter().enumerate() {
            if row.repository != repository || row.epoch != epoch {
                continue;
            }
            selected_bytes = selected_bytes
                .checked_add(row.info.size_bytes)
                .ok_or(CrabError::Limit(crate::LimitKind::CapturedCellBundleBytes))?;
            if row.info.size_bytes > self.limits.max_file_bytes
                || selected_bytes > self.limits.max_plan_bytes
            {
                return Err(CrabError::Limit(crate::LimitKind::CapturedCellBundleBytes));
            }
            prospective.push(SegmentDescriptor::bundled(
                row.info.clone(),
                [0; 32],
                0,
                bundle_digest,
                row.offset,
            ));
            let bytes = bundle.read_segment(index)?;
            let (file, size, digest, pages) = crate::ltx::inspect_bytes_with_index(&bytes)?;
            if size != row.info.size_bytes
                || digest != row.info.blake3
                || crate::SegmentInfo::from_inspected(&file, size, digest) != row.info
            {
                return Err(CrabError::ChecksumMismatch);
            }
            self.admit_segment_representation(&row.info, pages.len() * crate::paged::ENTRY_BYTES)?;
            let index_bytes = Bytes::from(crate::paged::encode_index_from_pages(&pages)?);
            inputs.push(AppendInput {
                info: row.info.clone(),
                location: BodyLocation::Bundle {
                    digest: bundle_digest,
                    offset: row.offset,
                },
                index: index_bytes,
                body: AppendBody::Bundle,
            });
        }
        let target = inputs
            .last()
            .map(|input| input.info.position())
            .ok_or(CrabError::TxNotAvailable)?;
        self.validate_chain(&prospective, target)?;
        self.prepare_append(
            base,
            base_graph,
            inputs,
            target,
            commit_sequence,
            schema,
            Some(bundle),
        )
        .await
    }

    /// Prepares an exact representation-only compaction of a pinned root.
    ///
    /// The output retains the base TXID, checksum, commit sequence and schema.
    /// Only the authority owner may later publish the proposal as a normal root CAS.
    /// `scratch_directory` must already exist, be private to the caller and have
    /// space for selected bodies and indexes plus compacted LTX/index outputs.
    /// Owned scratch files are removed after success or failure.
    pub async fn prepare_compaction(
        &self,
        base: &RootRef,
        range: std::ops::Range<usize>,
        level: u8,
        scratch_directory: &Path,
    ) -> Result<PreparedRoot> {
        let started = self.host.now_monotonic();
        let result = async {
            let mut replica = self.clone();
            replica.host = self.host.for_recovery().await?;
            let graph = replica.load_graph(base).await?;
            let scratch_bytes = compaction_scratch_bytes(&graph, range.clone())?;
            replica.host = replica.host.for_scratch(scratch_bytes).await?;
            compaction::prepare(&replica, base, graph, range, level, scratch_directory).await
        }
        .await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::Compaction, started, result.is_ok());
        result
    }

    /// Prepares one bounded level promotion, or an emergency full compaction.
    ///
    /// Normal promotions require eight contiguous inputs from the preceding
    /// level. A root near its segment or byte ceiling is compacted completely so
    /// the next append cannot strand an otherwise healthy writer at admission.
    pub async fn prepare_scheduled_compaction(
        &self,
        base: &RootRef,
        scratch_directory: &Path,
    ) -> Result<Option<PreparedRoot>> {
        let started = self.host.now_monotonic();
        let result = self
            .prepare_scheduled_compaction_inner(base, scratch_directory)
            .await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::Compaction, started, result.is_ok());
        result
    }

    async fn prepare_scheduled_compaction_inner(
        &self,
        base: &RootRef,
        scratch_directory: &Path,
    ) -> Result<Option<PreparedRoot>> {
        let mut replica = self.clone();
        replica.host = self.host.for_recovery().await?;
        let graph = replica.load_graph(base).await?;
        let segment_limit = MAX_SEGMENTS.min(replica.limits.max_segments);
        let stored_bytes = graph
            .descriptors
            .iter()
            .try_fold(0_u64, |total, descriptor| {
                total
                    .checked_add(descriptor.info.size_bytes)
                    .and_then(|value| value.checked_add(descriptor.index_length))
                    .ok_or(CrabError::Limit(crate::LimitKind::CellRootBytes))
            })?;
        let byte_pressure = stored_bytes >= replica.limits.max_plan_bytes.saturating_mul(3) / 4;
        let selected = if graph.descriptors.len() > 1
            && (graph.descriptors.len() >= segment_limit.saturating_sub(1).max(1) || byte_pressure)
        {
            let end = graph.descriptors.len();
            Some((0..end, 9))
        } else {
            let mut selected = None;
            for level in 1..=8 {
                if let Some(range) = scheduled_compaction_range(
                    &graph.descriptors,
                    level,
                    replica.limits.max_file_bytes,
                ) {
                    selected = Some((range, level));
                    break;
                }
            }
            selected
        };
        let Some((range, level)) = selected else {
            return Ok(None);
        };
        let scratch_bytes = compaction_scratch_bytes(&graph, range.clone())?;
        replica.host = replica.host.for_scratch(scratch_bytes).await?;
        compaction::prepare(&replica, base, graph, range, level, scratch_directory)
            .await
            .map(Some)
    }

    async fn prepare_append(
        &self,
        base: Option<&RootRef>,
        base_graph: Option<LoadedGraph>,
        inputs: Vec<AppendInput>,
        target: Position,
        commit_sequence: u64,
        schema: u32,
        bundle: Option<&crate::bundle::Bundle>,
    ) -> Result<PreparedRoot> {
        let prepared = inputs
            .into_iter()
            .map(|input| {
                let digest = *blake3::hash(&input.index).as_bytes();
                let descriptor = match input.location {
                    BodyLocation::Native => {
                        SegmentDescriptor::native(input.info, digest, input.index.len() as u64)
                    }
                    BodyLocation::Bundle { digest, offset } => SegmentDescriptor::bundled(
                        input.info,
                        *blake3::hash(&input.index).as_bytes(),
                        input.index.len() as u64,
                        digest,
                        offset,
                    ),
                };
                Ok(PreparedSegment {
                    descriptor,
                    index: input.index,
                    body: input.body,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut descriptors = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        descriptors.extend(prepared.iter().map(|segment| segment.descriptor.clone()));
        self.validate_chain(&descriptors, target)?;
        let directory_inputs = prepared
            .iter()
            .map(|segment| DirectoryInput {
                descriptor: segment.descriptor.clone(),
                index: segment.index.clone(),
            })
            .collect::<Vec<_>>();
        let dependency_uploads = async {
            if let Some(bundle) = bundle {
                self.put_bundle(bundle).await?;
            }
            stream::iter(
                prepared
                    .into_iter()
                    .map(|segment| self.upload_prepared_segment(segment)),
            )
            .buffered(SEGMENT_TRANSFER_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
            Ok::<(), CrabError>(())
        };
        let root_preparation = self.finish_preparation(
            base,
            base_graph,
            descriptors,
            &directory_inputs,
            target,
            commit_sequence,
            schema,
        );
        // Content-addressed dependencies and root metadata can upload in
        // parallel. The private proposal is returned only after both branches
        // finish, so a failed branch can leave only unreachable objects.
        let (_, prepared) =
            futures_util::future::try_join(dependency_uploads, root_preparation).await?;
        Ok(prepared)
    }

    async fn finish_preparation(
        &self,
        base: Option<&RootRef>,
        base_graph: Option<LoadedGraph>,
        descriptors: Vec<SegmentDescriptor>,
        directory_inputs: &[DirectoryInput],
        target: Position,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        for descriptor in &descriptors {
            descriptor.validate_published(self.limits)?;
        }
        let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
        let page_size = endpoint.info.page_size;
        let database_pages = endpoint.info.database_pages;
        let extents = object_extents(&descriptors)?;
        let directory = if let Some(graph) = &base_graph {
            let (changes, retain_through) =
                directory_changes(directory_inputs, graph.document.database_pages)?;
            let base_extents = object_extents(&graph.descriptors)?;
            DirectoryTree::update(
                directory::Verification {
                    layout: &self.layout,
                    cell: &self.cell,
                    incarnation: &self.incarnation,
                    page_size: graph.document.page_size,
                    database_pages: graph.document.database_pages,
                    extents: &base_extents,
                    host: &self.host,
                    origin: crate::LtxReadOrigin::Cold,
                },
                graph.document.directory_digest,
                graph.document.directory_height,
                graph.aggregate,
                changes,
                retain_through,
                directory::Verification {
                    layout: &self.layout,
                    cell: &self.cell,
                    incarnation: &self.incarnation,
                    page_size,
                    database_pages,
                    extents: &extents,
                    host: &self.host,
                    origin: crate::LtxReadOrigin::Cold,
                },
                target.checksum,
            )
            .await?
        } else {
            let entries = directory::initial_entries(directory_inputs)?;
            let directory =
                directory::build_initial_and_upload(entries, page_size, database_pages, self)
                    .await?;
            if directory.checksum() != target.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            directory
        };
        let directory_uploads = self.put_objects(
            CellObjectKind::Directory,
            directory
                .objects()
                .iter()
                .map(|node| (node.digest, node.bytes.clone()))
                .collect(),
        );
        let root_uploads = self.finish_root(
            base,
            descriptors,
            target,
            commit_sequence,
            schema,
            page_size,
            database_pages,
            directory,
        );
        // Both object sets are immutable; no proposal escapes unless every
        // upload succeeds, and a failed sibling leaves only unreachable data.
        let (_, prepared) = futures_util::future::try_join(directory_uploads, root_uploads).await?;
        Ok(prepared)
    }

    #[expect(clippy::too_many_arguments)]
    pub(super) async fn finish_root(
        &self,
        base: Option<&RootRef>,
        descriptors: Vec<SegmentDescriptor>,
        target: Position,
        commit_sequence: u64,
        schema: u32,
        page_size: u32,
        database_pages: u32,
        directory: DirectoryTree,
    ) -> Result<PreparedRoot> {
        if directory.checksum() != target.checksum {
            return Err(CrabError::ChecksumMismatch);
        }

        let mut segment_pages = Vec::new();
        let mut root_objects = Vec::new();
        for page in descriptors.chunks(SEGMENTS_PER_PAGE) {
            let bytes = encode_segment_page(page)?;
            let digest = *blake3::hash(&bytes).as_bytes();
            root_objects.push((digest, bytes));
            segment_pages.push(digest);
        }
        if segment_pages.len() > MAX_SEGMENT_PAGES {
            return Err(CrabError::Limit(crate::LimitKind::CellRootSegmentPages));
        }
        let document = RootDocument {
            cell: self.cell,
            checksum: target.checksum,
            commit_sequence,
            database_pages,
            directory_digest: directory.root_digest(),
            directory_height: directory.height(),
            incarnation: self.incarnation,
            page_size,
            schema,
            segment_pages,
            txid: target.txid,
        };
        let bytes = encode_root(&document)?;
        let digest = *blake3::hash(&bytes).as_bytes();
        root_objects.push((digest, bytes));
        // The document and its immutable segment pages can be uploaded in
        // parallel. The root digest remains private until all uploads finish.
        self.put_objects(CellObjectKind::Root, root_objects).await?;
        let root = RootRef {
            cell: self.cell,
            incarnation: self.incarnation,
            digest,
            position: target,
            commit_sequence,
        };
        let host = self
            .host
            .clone()
            .without_recovery()
            .without_dirty()
            .without_scratch();
        Ok(PreparedRoot {
            predecessor: base.copied(),
            verified: VerifiedRoot::from_graph(
                self.clone().with_host(host),
                root,
                &document,
                descriptors,
            )?,
        })
    }

    fn validate_metadata(&self, commit_sequence: u64, schema: u32) -> Result<()> {
        if schema == 0 || commit_sequence > i64::MAX as u64 {
            return Err(CrabError::InvalidState("invalid Cell root metadata"));
        }
        Ok(())
    }

    fn validate_append_sequence(
        &self,
        base: &Option<LoadedGraph>,
        commit_sequence: u64,
    ) -> Result<()> {
        if base
            .as_ref()
            .is_some_and(|graph| commit_sequence <= graph.document.commit_sequence)
        {
            return Err(CrabError::InvalidState("commit sequence did not advance"));
        }
        Ok(())
    }
}
