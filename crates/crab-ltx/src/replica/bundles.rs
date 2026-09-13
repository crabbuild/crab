//! Bundle publication shares the replica's exact-plan and conditional-head boundary.

use super::*;

impl Replica {
    /// Bundles a pinned plan and atomically replaces its exact transport locations.
    ///
    /// Native objects and old bundles remain retained. Readers use the manifest's
    /// location directly: transport errors never select an unverified fallback.
    pub async fn bundle(&self, expected: &ReplicaHead) -> Result<ReplicaHead> {
        self.check_head(expected)?;
        self.recovery_scope().await?.bundle_inner(expected).await
    }

    async fn bundle_inner(&self, expected: &ReplicaHead) -> Result<ReplicaHead> {
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
        self.put(
            &self.object_path(&hash, "bundle"),
            Bytes::copy_from_slice(bundle.bytes()),
        )
        .await?;
        let mut segments = expected.manifest.segments.clone();
        for (segment, row) in segments.iter_mut().zip(bundle.rows()) {
            let index = self.index(segment).await?;
            self.put(
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
        let repository = self.layout.repo_path("").to_string();
        let rows: Vec<_> = bundle
            .rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.repository == repository && row.epoch == self.epoch)
            .collect();
        if rows.is_empty() || bundle.bytes().len() as u64 > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle selection"));
        }
        let target = rows
            .last()
            .ok_or(CrabError::TxNotAvailable)?
            .1
            .info
            .position();
        self.admit_append(
            expected,
            rows.iter().map(|(_, row)| row.info.clone()),
            target,
        )?;
        let hash = *blake3::hash(bundle.bytes()).as_bytes();
        let inputs = rows
            .into_iter()
            .map(|(index, row)| {
                Ok((
                    bundle.segment(index)?.to_vec(),
                    row.info.clone(),
                    Some(BundleLocation {
                        hash,
                        offset: row.offset,
                        size: bundle.bytes().len() as u64,
                    }),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let (prepared, pages) = self.prepare_append(inputs, expected, target).await?;
        self.put(
            &self.object_path(&hash, "bundle"),
            Bytes::copy_from_slice(bundle.bytes()),
        )
        .await?;
        self.publish_append(prepared, pages, expected).await
    }
}
