use super::*;

impl PagedGitObjectIndex {
    /// Capture an authenticated descriptor without loading its physical inventory or locations.
    ///
    /// Callers accept readiness only after complete verification; placement never authorizes refs.
    pub async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        hash: &str,
        state_digest: &str,
        manifest: &Manifest,
        maximum: u64,
    ) -> Result<Self> {
        validate_content_hash(hash, "object index descriptor", "Git object index")?;
        let expected = hash_bytes(hash)?;
        let bytes = store
            .verify_bounded(
                &layout.bulk_manifest_path("git-object-index", hash),
                &expected,
                maximum.min(MAX_DESCRIPTOR_BYTES),
            )
            .await?;
        let descriptor: Descriptor = serde_json::from_slice(&bytes)
            .map_err(|source| crate::error::MetadataError::GitObjectIndexRecord { source })?;
        descriptor.validate()?;
        if descriptor.state_digest != state_digest
            || descriptor.generation != manifest.generation
            || descriptor.pack_index_hash != manifest.pack_index_hash
            || descriptor.git_validation_digest != manifest.git_validation_digest
        {
            return bad("object index does not match captured state");
        }
        let path = body_path(layout, &descriptor.body_hash);
        Ok(Self {
            descriptor,
            descriptor_bytes: bytes.len() as u64,
            path,
            origin: Arc::clone(layout.store().inner()),
            global_prefix: layout.global_prefix().to_owned(),
            maximum,
            cache: PageCache::default(),
        })
    }

    /// Check captured state and in-process origin before attaching lookup to a repository.
    #[must_use]
    pub fn matches_binding(
        &self,
        layout: &StoreLayout<Store>,
        state_digest: &str,
        manifest: &Manifest,
    ) -> bool {
        Arc::ptr_eq(&self.origin, layout.store().inner())
            && self.global_prefix == layout.global_prefix()
            && self.path == body_path(layout, &self.descriptor.body_hash)
            && self.descriptor.state_digest == state_digest
            && self.descriptor.generation == manifest.generation
            && self.descriptor.pack_index_hash == manifest.pack_index_hash
            && self.descriptor.git_validation_digest == manifest.git_validation_digest
    }

    /// Return the immutable body containing every page for retained-root GC.
    #[must_use]
    pub fn body_path(&self) -> &object_store::path::Path {
        &self.path
    }

    /// Resolve OIDs against the captured physical inventory, preserving duplicates and misses.
    ///
    /// The caller admits request/output counts and supplies an admitted store for the captured origin.
    pub async fn lookup(
        &self,
        store: &Store,
        oids: &[[u8; 20]],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<Option<GitObjectLocator>>> {
        self.lookup_with_work(store, oids, inventory, &mut self.work())
            .await
    }

    async fn lookup_with_work(
        &self,
        store: &Store,
        oids: &[[u8; 20]],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        work: &mut PageWork,
    ) -> Result<Vec<Option<GitObjectLocator>>> {
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let values = self
            .lookup_rows(
                store,
                &self.descriptor.objects,
                Kind::ObjectLocators,
                &keys,
                work,
            )
            .await?;
        let pointers = values
            .into_iter()
            .map(|value| value.map(|value| decode_pointer(&value)).transpose())
            .collect::<Result<Vec<_>>>()?;
        let slots = pointers
            .iter()
            .flatten()
            .map(|pointer| pointer.slot)
            .collect::<Vec<_>>();
        let packs = self.packs_for_slots(store, &slots, inventory, work).await?;
        let keys = pointers
            .iter()
            .flatten()
            .map(|pointer| offset_key(pointer.slot, pointer.offset))
            .collect::<Vec<_>>();
        let rows = self
            .lookup_rows(
                store,
                &self.descriptor.offsets,
                Kind::PackOffsets,
                &keys,
                work,
            )
            .await?;
        let mut rows = rows.into_iter();
        let mut output = Vec::with_capacity(oids.len());
        for (oid, pointer) in oids.iter().zip(pointers) {
            let Some(pointer) = pointer else {
                output.push(None);
                continue;
            };
            let row = rows
                .next()
                .flatten()
                .ok_or_else(|| corruption("object pointer has no physical offset"))?;
            if array::<20>(&row, 0)? != *oid {
                return bad("object and physical offset identities differ");
            }
            let pack = packs
                .get(&pointer.slot)
                .ok_or_else(|| corruption("primary pack slot is absent"))?;
            let (location, mut metadata) = decode_location(
                pointer.offset,
                row.get(20..)
                    .ok_or_else(|| corruption("truncated offset row"))?,
            )?;
            if !format::validate_location_for_pack(location, pack.pack_size)
                || logical_conflict(metadata, pointer.logical)
            {
                return bad("physical location or logical facts differ");
            }
            // Logical facts can be proven by another alias. Encoding-specific
            // base facts remain solely in the selected physical record.
            metadata.kind = pointer.logical.kind.or(metadata.kind);
            metadata.logical_size = pointer.logical.logical_size.or(metadata.logical_size);
            output.push(Some(GitObjectLocator {
                ordinal: 0,
                pack_id: pack.pack_id,
                location,
                metadata,
            }));
        }
        Ok(output)
    }

    /// Resolve physical delta-base offsets without reading an entire reverse pack index.
    ///
    /// The caller admits request/output counts and supplies an admitted store for the captured origin.
    pub async fn oids_at_offsets(
        &self,
        store: &Store,
        offsets: &[(MerkleHash, u64)],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<Option<[u8; 20]>>> {
        let mut work = self.work();
        let pack_ids = offsets.iter().map(|(pack, _)| *pack).collect::<Vec<_>>();
        let slots = self
            .slots_for_packs(store, &pack_ids, inventory, &mut work)
            .await?;
        let keys = slots
            .iter()
            .zip(offsets)
            .map(|(slot, (_, offset))| offset_key(*slot, *offset))
            .collect::<Vec<_>>();
        let rows = self
            .lookup_rows(
                store,
                &self.descriptor.offsets,
                Kind::PackOffsets,
                &keys,
                &mut work,
            )
            .await?;
        let mut output = Vec::with_capacity(rows.len());
        let mut physical_facts = Vec::new();
        for ((pack_id, offset), row) in offsets.iter().zip(rows) {
            let Some(row) = row else {
                output.push(None);
                continue;
            };
            let oid = array(&row, 0)?;
            let (location, metadata) = decode_location(
                *offset,
                row.get(20..)
                    .ok_or_else(|| corruption("truncated offset row"))?,
            )?;
            if !inventory
                .get(pack_id)
                .is_some_and(|pack| format::validate_location_for_pack(location, pack.pack_size))
            {
                return bad("location escapes captured pack");
            }
            output.push(Some(oid));
            physical_facts.push(metadata);
        }
        let oids = output.iter().flatten().copied().collect::<Vec<_>>();
        let primary = self
            .lookup_with_work(store, &oids, inventory, &mut work)
            .await?;
        for (physical, primary) in physical_facts.into_iter().zip(primary) {
            let primary =
                primary.ok_or_else(|| corruption("physical offset has no object identity"))?;
            if logical_conflict(physical, primary.metadata) {
                return bad("physical aliases disagree about logical facts");
            }
        }
        Ok(output)
    }

    /// Verify complete coverage and canonical bytes before publishing readiness.
    pub async fn verify_complete(
        &self,
        store: &Store,
        manifest: &Manifest,
        maximum: u64,
    ) -> Result<()> {
        let bytes = store
            .verify_bounded(
                &self.path,
                &hash_bytes(&self.descriptor.body_hash)?,
                maximum,
            )
            .await?;
        if bytes.len() as u64 != self.descriptor.body_bytes {
            return bad("object index body length mismatch");
        }
        let mut audit = BTreeMap::new();
        crate::pages::scan(
            &bytes,
            &self.descriptor.objects,
            Kind::ObjectLocators,
            self.path.as_ref(),
            &mut audit,
        )?;
        crate::pages::scan(
            &bytes,
            &self.descriptor.packs,
            Kind::PackInventory,
            self.path.as_ref(),
            &mut audit,
        )?;
        let slots = crate::pages::scan(
            &bytes,
            &self.descriptor.slots,
            Kind::PackSlots,
            self.path.as_ref(),
            &mut audit,
        )?;
        let inventory = slots
            .into_iter()
            .map(|(_, value)| decode_pack(&value))
            .collect::<Result<Vec<_>>>()?;
        let offsets = crate::pages::scan(
            &bytes,
            &self.descriptor.offsets,
            Kind::PackOffsets,
            self.path.as_ref(),
            &mut audit,
        )?;
        let entries = offsets
            .into_iter()
            .map(|(key, value)| {
                let slot = u32::from_be_bytes(array(&key, 0)?);
                let pack = inventory
                    .get(slot as usize)
                    .ok_or_else(|| corruption("physical pack slot is absent"))?;
                let offset = u64::from_be_bytes(array(&key, 4)?);
                let (location, metadata) = decode_location(
                    offset,
                    value
                        .get(20..)
                        .ok_or_else(|| corruption("truncated offset row"))?,
                )?;
                Ok((
                    array(&value, 0)?,
                    GitObjectLocator {
                        ordinal: 0,
                        pack_id: pack.pack_id,
                        location,
                        metadata,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let rebuilt = PagedGitObjectIndexWrite::build(
            &self.descriptor.state_digest,
            manifest,
            &inventory,
            &entries,
        )?;
        if rebuilt.descriptor != self.descriptor || rebuilt.body.as_ref() != bytes.as_ref() {
            return bad("object index coverage or canonical placement mismatch");
        }
        Ok(())
    }

    fn work(&self) -> PageWork {
        PageWork {
            maximum: self.maximum,
            bytes: self.descriptor_bytes,
            seen: BTreeSet::new(),
        }
    }

    async fn lookup_rows(
        &self,
        store: &Store,
        root: &PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.cache
            .lookup(
                store,
                &self.path,
                0,
                self.descriptor.body_bytes,
                root,
                kind,
                keys,
                work,
            )
            .await
    }

    async fn slot_packs(
        &self,
        store: &Store,
        slots: &[u32],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        work: &mut PageWork,
    ) -> Result<BTreeMap<u32, GitPackInventoryEntry>> {
        let slots = slots.iter().copied().collect::<BTreeSet<_>>();
        let keys = slots
            .iter()
            .map(|slot| slot.to_be_bytes().to_vec())
            .collect::<Vec<_>>();
        let rows = self
            .lookup_rows(store, &self.descriptor.slots, Kind::PackSlots, &keys, work)
            .await?;
        slots
            .into_iter()
            .zip(rows)
            .map(|(slot, row)| {
                let row = row.ok_or_else(|| corruption("required pack slot is absent"))?;
                let pack = decode_pack(&row)?;
                if inventory.get(&pack.pack_id) != Some(&pack) {
                    return bad("pack slot differs from captured inventory");
                }
                Ok((slot, pack))
            })
            .collect()
    }

    async fn packs_for_slots(
        &self,
        store: &Store,
        slots: &[u32],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        work: &mut PageWork,
    ) -> Result<BTreeMap<u32, GitPackInventoryEntry>> {
        let packs = self.slot_packs(store, slots, inventory, work).await?;
        let keys = packs
            .values()
            .map(|pack| <[u8; 32]>::from(pack.pack_id).to_vec())
            .collect::<Vec<_>>();
        let rows = self
            .lookup_rows(
                store,
                &self.descriptor.packs,
                Kind::PackInventory,
                &keys,
                work,
            )
            .await?;
        for ((slot, pack), row) in packs.iter().zip(rows) {
            let row = row.ok_or_else(|| corruption("required pack binding is absent"))?;
            if decode_binding(pack.pack_id, &row)? != (*slot, *pack) {
                return bad("pack directories disagree");
            }
        }
        Ok(packs)
    }

    async fn slots_for_packs(
        &self,
        store: &Store,
        ids: &[MerkleHash],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        work: &mut PageWork,
    ) -> Result<Vec<u32>> {
        let keys = ids
            .iter()
            .map(|id| <[u8; 32]>::from(*id).to_vec())
            .collect::<Vec<_>>();
        let rows = self
            .lookup_rows(
                store,
                &self.descriptor.packs,
                Kind::PackInventory,
                &keys,
                work,
            )
            .await?;
        let slots = ids
            .iter()
            .zip(rows)
            .map(|(id, row)| {
                let row = row.ok_or_else(|| corruption("required pack binding is absent"))?;
                let (slot, pack) = decode_binding(*id, &row)?;
                if inventory.get(id) != Some(&pack) {
                    return bad("pack binding differs from captured inventory");
                }
                Ok(slot)
            })
            .collect::<Result<Vec<_>>>()?;
        // The forward binding is already authenticated above. Read its reverse once;
        // repeating the forward traversal adds work for every offset-delta base.
        let packs = self.slot_packs(store, &slots, inventory, work).await?;
        for (id, slot) in ids.iter().zip(&slots) {
            if packs.get(slot).is_none_or(|pack| pack.pack_id != *id) {
                return bad("pack directories disagree");
            }
        }
        Ok(slots)
    }
}
