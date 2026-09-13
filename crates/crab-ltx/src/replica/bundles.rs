//! Bundle publication shares the replica's exact-plan and conditional-head boundary.

use super::*;

impl Replica {
    /// Bundles a pinned plan and atomically replaces its exact transport locations.
    ///
    /// Native objects and old bundles remain retained. Readers use the manifest's
    /// location directly: transport errors never select an unverified fallback.
    pub async fn bundle(&self, expected: &ReplicaHead) -> Result<ReplicaHead> {
        if expected.etag.is_none() {
            return Err(CrabError::InvalidState("historical head is read-only"));
        }
        let plan = self.verified(expected).await?;
        let repository = self.layout.repo_path("").to_string();
        let entries = plan
            .inputs
            .into_iter()
            .zip(&expected.manifest.segments)
            .map(|(bytes, s)| crate::bundle::BundleEntry {
                repository: repository.clone(),
                epoch: s.epoch.clone(),
                info: s.info.clone(),
                bytes,
            })
            .collect();
        let limits = self.limits;
        let bundle = self
            .host
            .run(move || crate::bundle::Bundle::encode(entries, limits))
            .await??;
        let hash = *blake3::hash(bundle.bytes()).as_bytes();
        self.layout
            .store()
            .put(
                &self.object_path(&hash, "bundle"),
                Bytes::copy_from_slice(bundle.bytes()),
            )
            .await?;
        let mut segments = expected.manifest.segments.clone();
        for (segment, row) in segments.iter_mut().zip(bundle.rows()) {
            let index = self.index(segment).await?;
            self.layout
                .store()
                .put(
                    &self.object_path(&segment.index_hash, "idx"),
                    Bytes::from(index),
                )
                .await?;
            segment.epoch = self.epoch.clone();
            segment.bundle = Some(BundleLocation {
                hash,
                offset: row.offset,
                size: bundle.bytes().len() as u64,
            });
        }
        self.publish(segments, expected.position(), Some(expected))
            .await
    }

    /// Publishes this repository/epoch's captured rows directly from a verified bundle.
    ///
    /// No standalone LTX upload is required. A host may aggregate captures from
    /// several repositories, but each head is published independently, not as
    /// an atomic multi-repository transaction. The bundle is retained in this
    /// replica's namespace so its retention never depends on a different tenant.
    pub async fn replicate_bundle(
        &self,
        bundle: &crate::bundle::Bundle,
        expected: Option<&ReplicaHead>,
    ) -> Result<ReplicaHead> {
        if let Some(head) = expected {
            self.check_head(head)?;
            if head.etag.is_none() {
                return Err(CrabError::InvalidState("historical head is read-only"));
            }
        }
        let mut segments = expected
            .map(|h| h.manifest.segments.clone())
            .unwrap_or_default();
        let mut inputs = self.download(&segments).await?;
        let mut infos: Vec<_> = segments.iter().map(|s| s.info.clone()).collect();
        let repository = self.layout.repo_path("").to_string();
        let rows: Vec<_> = bundle
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.repository == repository && row.epoch == self.epoch)
            .collect();
        if rows.is_empty()
            || rows.len().saturating_add(segments.len()) > self.limits.max_segments
            || bundle.bytes().len() as u64 > self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit("bundle selection"));
        }
        let target = rows
            .last()
            .ok_or(CrabError::TxNotAvailable)?
            .1
            .info
            .position();
        for (index, row) in &rows {
            inputs.push(bundle.segment(*index)?.to_vec());
            infos.push(row.info.clone());
        }
        let limits = self.limits;
        self.host
            .run(move || VerifiedLocalPlan::from_bytes(inputs, &infos, target, limits))
            .await??;
        let hash = *blake3::hash(bundle.bytes()).as_bytes();
        self.layout
            .store()
            .put(
                &self.object_path(&hash, "bundle"),
                Bytes::copy_from_slice(bundle.bytes()),
            )
            .await?;
        for (index, row) in rows {
            let bytes = bundle.segment(index)?.to_vec();
            let index = self
                .host
                .run(move || crate::paged::encode_index(&bytes))
                .await??;
            let index_hash = *blake3::hash(&index).as_bytes();
            let index_size = index.len() as u64;
            self.layout
                .store()
                .put(&self.object_path(&index_hash, "idx"), Bytes::from(index))
                .await?;
            segments.push(RemoteSegment {
                epoch: self.epoch.clone(),
                level: 0,
                bundle: Some(BundleLocation {
                    hash,
                    offset: row.offset,
                    size: bundle.bytes().len() as u64,
                }),
                info: row.info.clone(),
                index_hash,
                index_size,
            });
        }
        self.publish(segments, target, expected).await
    }
}
