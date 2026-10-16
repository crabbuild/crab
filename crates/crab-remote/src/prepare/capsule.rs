use super::*;

impl Prepared {
    /// Upload artifacts for one captured capsule-protocol repository view.
    ///
    /// The caller must hold ref leases and GC fences from before opening
    /// `view` through commitment. This validates all historical dependencies
    /// against that view, uploads same-publication Xet content before Git
    /// visibility, and retains the exact root snapshot for per-ref CAS.
    pub async fn upload_capsule(
        &self,
        view: &crab_read::capsule_protocol::CapsuleRepositoryView,
        limits: crab_read::dependency_proof::DependencyProofLimits,
        cancel: &CancellationToken,
    ) -> Result<CapsuleArtifacts> {
        let started = Instant::now();
        let repository_refs = self
            .repository
            .refs()
            .entries
            .iter()
            .map(|reference| (reference.name.clone(), reference.target.to_string()))
            .collect::<BTreeMap<_, _>>();
        if repository_refs != *view.refs()
            || self.repository.generation() != view.root().root().generation()
        {
            return Err(Error::Request(
                "Capsule view differs from the prepared repository version",
            ));
        }
        if self.plan.pointers().len() > limits.max_dependencies {
            return Err(Error::Dependency(Box::new(
                crab_read::dependency_proof::DependencyProofError::Limit("pointer count"),
            )));
        }
        let lookup = view.pointer_catalog_lookup(
            &self.layout,
            crab_metadata::capsule_protocol::MAX_CATALOG_BYTES,
        )?;
        let hashes = self
            .plan
            .pointers()
            .iter()
            .filter_map(|dependency| match &dependency.pointer {
                crab_git::pointer_detect::PointerKind::Crab(pointer) => {
                    Some(crab_xet::hash::MerkleHash::from(pointer.file_hash).hex())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        // Ordinary Git and LFS-only changes need no Xet catalog pages. Selected
        // Crab pointers retain complete authenticated shard/xorb closure checks.
        let base_catalog = if hashes.is_empty() {
            PointerCatalog::new()
        } else {
            lookup.files(self.layout.store(), &hashes).await?
        };
        let prepared_files = self
            .content
            .as_ref()
            .map(|content| {
                content
                    .files
                    .iter()
                    .map(|file| file.file_hash)
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default();
        crab_read::dependency_proof::verify_capsule_dependencies_except_crab(
            &self.layout,
            &base_catalog,
            self.plan.pointers(),
            &prepared_files,
            limits,
            cancel,
        )
        .await
        .map_err(|error| Error::Dependency(Box::new(error)))?;

        let (pointer_delta, origins) = match &self.content {
            Some(content) => {
                upload_capsule_content(&self.layout, &lookup, base_catalog, content, cancel).await?
            }
            None => (PointerCatalog::new(), HashMap::new()),
        };
        let mut packs = Vec::new();
        if let Some(pack) = &self.pack {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let (pack_bytes, index, reverse_index, locator) = tokio::try_join!(
                tokio::fs::read(pack.pack_path()),
                tokio::fs::read(pack.index_path()),
                tokio::fs::read(pack.reverse_path()),
                tokio::fs::read(pack.kinds_path()),
            )?;
            let external_delta_bases = pack
                .external_delta_bases()
                .iter()
                .map(ToString::to_string)
                .collect();
            packs.push(
                crab_metadata::capsule_protocol::CapsuleGitPack::new_with_external_delta_bases(
                    Bytes::from(pack_bytes),
                    Bytes::from(index),
                    Bytes::from(reverse_index),
                    Bytes::from(locator),
                    pack.git_sha1().to_string(),
                    u64::from(pack.object_count()),
                    external_delta_bases,
                )?,
            );
        }
        let mut sections = Vec::with_capacity(1);
        if !self.visibility.is_empty() {
            let visibility = crab_metadata::capsule_protocol::CapsuleVisibilityDelta::new(
                self.visibility.clone(),
            )?;
            sections.push(crab_metadata::capsule_protocol::CapsuleSection::new(
                crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                visibility.encode()?,
            ));
        }
        let edits = self
            .updates
            .iter()
            .map(|update| {
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    update.name.clone(),
                    update.old.map(|oid| oid.to_string()),
                    update.new.map(|oid| oid.to_string()),
                    self.plan
                        .peeled()
                        .get(&update.name)
                        .map(ToString::to_string),
                )
            })
            .collect::<Vec<_>>();
        let changes_namespace = edits
            .iter()
            .any(|edit| edit.expected_old().is_none() != edit.new_oid().is_none());
        tracing::info!(
            phase = "capsule_input",
            packs = packs.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Git receive phase completed"
        );
        Ok(CapsuleArtifacts {
            layout: self.layout.clone(),
            base: view.root_snapshot().clone(),
            edits,
            packs,
            sections,
            pointer_delta,
            origins,
            changes_namespace,
        })
    }
}

impl CapsuleArtifacts {
    /// Commit through independently mutable ref heads and one capsule authority.
    ///
    /// Revalidate authorization and repository policy immediately before this
    /// call. The caller must retain its original ref leases and GC fences until
    /// the returned outcome is known and all lease cleanup has completed.
    pub async fn commit(
        self,
        plan_id: Option<&str>,
        namespace_ttl: Duration,
        cancel: &CancellationToken,
    ) -> Result<CapsuleCommitOutcome> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let transaction = match plan_id {
            Some(plan_id) => crab_metadata::capsule_protocol::CapsuleTransaction::for_plan(
                self.base.record().digest(),
                plan_id,
                self.edits,
            )?,
            None => crab_metadata::capsule_protocol::CapsuleTransaction::new(
                self.base.record().digest(),
                self.edits,
            )?,
        };
        let transaction_id = transaction.id()?;
        let started = Instant::now();
        let mut sections = self.sections;
        if !self.pointer_delta.is_empty() {
            sections.insert(
                0,
                crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
                    self.pointer_delta.encode_delta()?,
                ),
            );
        }
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, self.packs, sections)?;
        tracing::info!(
            phase = "capsule_encode",
            %transaction_id,
            bytes = capsule.bytes().len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Git receive phase completed"
        );
        let publish = if self.changes_namespace {
            let ref_names = transaction
                .edits()
                .iter()
                .map(|edit| edit.ref_name().to_owned())
                .collect::<Vec<_>>();
            let layout = self.layout.clone();
            let commit_layout = layout.clone();
            let base = self.base;
            crab_write::with_ref_namespaces(
                layout.store(),
                &layout,
                &ref_names,
                namespace_ttl,
                cancel,
                move |scoped| async move {
                    if scoped.is_cancelled() {
                        return Err(crab_write::WriteError::Cancelled);
                    }
                    crab_write::capsule_protocol::validate_ref_namespace(
                        &commit_layout,
                        base.record().root(),
                        transaction.edits(),
                    )
                    .await?;
                    let prepared = crab_write::capsule_protocol::prepare_publication(
                        &commit_layout,
                        base,
                        &transaction,
                        &capsule,
                    )
                    .await?;
                    // Namespace renewal joins caller cancellation. Recheck that
                    // scoped token after preparation, then drain visibility.
                    if scoped.is_cancelled() {
                        return Err(crab_write::WriteError::Cancelled);
                    }
                    crab_write::capsule_protocol::publish_prepared(&commit_layout, prepared).await
                },
            )
            .await
        } else {
            let prepared = crab_write::capsule_protocol::prepare_publication(
                &self.layout,
                self.base,
                &transaction,
                &capsule,
            )
            .await;
            match prepared {
                Ok(_) if cancel.is_cancelled() => Err(crab_write::WriteError::Cancelled),
                Ok(prepared) => {
                    crab_write::capsule_protocol::publish_prepared(&self.layout, prepared).await
                }
                Err(error) => Err(error),
            }
        };
        match publish {
            Ok(_) => {
                // Ref visibility owns the payload before hints become discoverable.
                // Failed hint maintenance cannot reject an already committed Git push.
                if let Err(error) = crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
                    &self.layout,
                    &self.pointer_delta,
                    &self.origins,
                    &|| cancel.is_cancelled(),
                )
                .await
                {
                    tracing::warn!(%error, "capsule committed; chunk candidate index needs repair");
                }
                Ok(CapsuleCommitOutcome::Committed { transaction_id })
            }
            Err(error @ crab_write::WriteError::CapsuleCommitUncertain { .. }) => {
                Ok(CapsuleCommitOutcome::Indeterminate {
                    transaction_id,
                    source: Box::new(error),
                })
            }
            Err(error) => Err(error.into()),
        }
    }
}

async fn upload_capsule_content(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    lookup: &PointerCatalogLookup,
    mut base: PointerCatalog,
    content: &PreparedContent,
    cancel: &CancellationToken,
) -> Result<(
    PointerCatalog,
    HashMap<crab_xet::hash::MerkleHash, crab_metadata::receipts::OriginReceipt>,
)> {
    let local = content.pointer_catalog();
    let mut delta = PointerCatalog::new();
    let mut origins = HashMap::new();
    let mut needed_shards = std::collections::BTreeSet::new();
    for (file_hash, file) in local.files() {
        if let Some(existing) = base.files().get(file_hash) {
            if existing.size() != file.size() {
                return Err(Error::Content(format!(
                    "file {file_hash} conflicts with the captured pointer catalog"
                )));
            }
            continue;
        }
        needed_shards.insert(file.shard_hash().to_owned());
        delta.insert_file(file_hash.clone(), file.clone())?;
    }
    if !needed_shards.is_empty() {
        base.apply(
            &lookup
                .shards(
                    layout.store(),
                    &needed_shards.iter().cloned().collect::<Vec<_>>(),
                )
                .await?,
        )?;
    }
    let mut needed_xorbs = std::collections::BTreeSet::new();
    for shard_hash in &needed_shards {
        let shard = local
            .shards()
            .get(shard_hash)
            .ok_or_else(|| Error::Content("prepared shard catalog entry is missing".to_owned()))?;
        if let Some(existing) = base.shards().get(shard_hash) {
            if existing != shard {
                return Err(Error::Content(format!(
                    "shard {shard_hash} conflicts with the captured pointer catalog"
                )));
            }
            continue;
        }
        needed_xorbs.extend(shard.xorb_hashes().iter().cloned());
        delta.insert_shard(shard_hash.clone(), shard.clone())?;
    }
    if !needed_xorbs.is_empty() {
        base.apply(
            &lookup
                .xorbs(
                    layout.store(),
                    &needed_xorbs.iter().cloned().collect::<Vec<_>>(),
                )
                .await?,
        )?;
    }
    let xorb_artifacts = content
        .xorbs
        .iter()
        .map(|artifact| {
            (
                crab_xet::hash::MerkleHash::from(artifact.protocol_hash).hex(),
                artifact,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for xorb_hash in needed_xorbs {
        let local_entry = local
            .xorbs()
            .get(&xorb_hash)
            .ok_or_else(|| Error::Content("prepared xorb catalog entry is missing".to_owned()))?;
        if let Some(existing) = base.xorbs().get(&xorb_hash) {
            if existing.chunks() != local_entry.chunks() {
                return Err(Error::Content(format!(
                    "xorb {xorb_hash} conflicts with the captured pointer catalog"
                )));
            }
            continue;
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let artifact = xorb_artifacts
            .get(&xorb_hash)
            .ok_or_else(|| Error::Content("prepared xorb artifact is missing".to_owned()))?;
        let bytes = artifact
            .read_verified(crab_xet::xorb::format::MAX_XORB_SIZE as u64, cancel)
            .await?;
        let hash = crab_xet::hash::MerkleHash::from(artifact.protocol_hash);
        let path = layout.xorb_path(&hash);
        let payload_digest = crab_xet::xorb::parser::XorbParser::parse(bytes.clone())
            .map_err(Error::ContentFormat)?
            .payload_digest();
        let (entry, payload_digest, identity) = match layout
            .store()
            .create_or_read_immutable(&path, bytes, crab_xet::xorb::format::MAX_XORB_SIZE as u64)
            .await?
        {
            crab_storage::ImmutableCreateOutcome::Created(identity) => {
                (local_entry.clone(), payload_digest, identity)
            }
            crab_storage::ImmutableCreateOutcome::Existing {
                bytes: existing,
                identity,
            } => {
                let expected = local_entry.clone();
                let expected_hash = xorb_hash.clone();
                let (entry, payload_digest) = tokio::task::spawn_blocking(move || {
                    catalog_xorb_from_existing(&expected_hash, &expected, existing)
                })
                .await??;
                (entry, payload_digest, identity)
            }
        };
        origins.insert(
            hash,
            crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                path.to_string(),
                hash.into(),
                payload_digest,
                entry.encoded_size(),
                identity.e_tag,
                identity.version,
            ),
        );
        delta.insert_xorb(xorb_hash, entry)?;
    }
    let shard_artifacts = content
        .shards
        .iter()
        .map(|artifact| {
            (
                crab_xet::hash::MerkleHash::from(artifact.protocol_hash).hex(),
                artifact,
            )
        })
        .collect::<BTreeMap<_, _>>();
    for shard_hash in needed_shards {
        if base.shards().contains_key(&shard_hash) {
            continue;
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let artifact = shard_artifacts
            .get(&shard_hash)
            .ok_or_else(|| Error::Content("prepared shard artifact is missing".to_owned()))?;
        let bytes = artifact
            .read_verified(crab_xet::shard::MAX_PUSH_SHARD_SIZE_BYTES, cancel)
            .await?;
        layout
            .store()
            .put_if_absent_verified(
                &layout.shard_path(&crab_xet::hash::MerkleHash::from(artifact.protocol_hash)),
                bytes,
            )
            .await?;
    }
    if !delta.shards().is_empty() {
        crab_metadata::ref_registry::union_register_repo_shards(
            layout.store(),
            layout,
            delta.shards().keys().cloned().collect(),
        )
        .await?;
    }
    base.apply(&delta)?;
    Ok((delta, origins))
}

fn catalog_xorb_from_existing(
    xorb_hash: &str,
    local: &XorbCatalogEntry,
    bytes: Bytes,
) -> Result<(XorbCatalogEntry, [u8; 32])> {
    let parser =
        crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).map_err(Error::ContentFormat)?;
    parser
        .verify_payload_digest()
        .map_err(Error::ContentFormat)?;
    parser.verify_all_chunks().map_err(Error::ContentFormat)?;
    if parser.hash().hex() != xorb_hash {
        return Err(Error::Content(
            "existing xorb has the wrong logical identity".to_owned(),
        ));
    }
    let chunks = (0..parser.num_chunks())
        .map(|index| {
            parser
                .chunk_meta(index)
                .map(|chunk| XorbChunkEntry::new(chunk.hash.hex(), chunk.uncompressed_len))
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Error::ContentFormat)?;
    if chunks != local.chunks() {
        return Err(Error::Content(
            "existing xorb has a conflicting chunk layout".to_owned(),
        ));
    }
    Ok((
        XorbCatalogEntry::new(
            bytes.len() as u64,
            blake3::hash(&bytes).to_hex().to_string(),
            chunks,
        ),
        parser.payload_digest(),
    ))
}
