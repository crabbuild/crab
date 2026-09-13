//! Verified append state shared by native and bundled publication.

use super::*;

pub(super) struct PreparedSegment {
    pub segment: RemoteSegment,
    pub bytes: Vec<u8>,
    pub index: Vec<u8>,
}

impl Replica {
    // Admit the entire prospective chain before reading local files or remote
    // indexes. Exact index sizes are checked again after encoding, before PUT.
    pub(super) fn admit_append(
        &self,
        expected: Option<&ReplicaHead>,
        infos: impl Iterator<Item = SegmentInfo>,
        target: Position,
    ) -> Result<()> {
        if let Some(head) = expected {
            self.check_head(head)?;
            if head.etag.is_none() {
                return Err(CrabError::InvalidState("historical head is read-only"));
            }
        }
        let mut segments = expected
            .map(|h| h.manifest.segments.clone())
            .unwrap_or_default();
        segments.extend(infos.map(|info| RemoteSegment {
            epoch: self.epoch.clone(),
            level: 0,
            bundle: None,
            info,
            index_hash: [0; 32],
            index_size: 0,
        }));
        self.validate_manifest(&Manifest {
            version: 2,
            epoch: self.epoch.clone(),
            position: target,
            segments,
            parent: expected.and_then(|h| h.manifest.parent.clone()),
        })
    }

    pub(super) async fn prepare_append(
        &self,
        inputs: Vec<(Vec<u8>, SegmentInfo, Option<BundleLocation>)>,
        expected: Option<&ReplicaHead>,
        target: Position,
    ) -> Result<(Vec<PreparedSegment>, crate::PagedDatabase)> {
        let limits = self.limits;
        let epoch = self.epoch.clone();
        let prepared = self
            .host
            .run(move || {
                inputs
                    .into_iter()
                    .map(|(bytes, info, bundle)| {
                        crate::recovery::verify_segment(&bytes, &info, limits)?;
                        let index = crate::paged::encode_index(&bytes)?;
                        let segment = RemoteSegment {
                            epoch: epoch.clone(),
                            level: 0,
                            bundle,
                            info,
                            index_hash: *blake3::hash(&index).as_bytes(),
                            index_size: index.len() as u64,
                        };
                        Ok(PreparedSegment {
                            segment,
                            bytes,
                            index,
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await??;
        let mut segments = expected
            .map(|h| h.manifest.segments.clone())
            .unwrap_or_default();
        segments.extend(prepared.iter().map(|p| p.segment.clone()));
        self.validate_manifest(&Manifest {
            version: 2,
            epoch: self.epoch.clone(),
            position: target,
            segments,
            parent: expected.and_then(|h| h.manifest.parent.clone()),
        })?;
        let base = match expected {
            Some(head) => Some(match &head.pages {
                Some(pages) if pages.belongs_to(self) => pages.clone(),
                _ => self.paged(head).await?,
            }),
            None => None,
        };
        let indexes = prepared
            .iter()
            .map(|p| (p.segment.clone(), p.index.clone()))
            .collect();
        let replica = self.clone();
        let pages = self
            .host
            .run(move || crate::paged::extend(replica, base, indexes, target))
            .await??;
        Ok((prepared, pages))
    }

    pub(super) async fn publish_append(
        &self,
        prepared: Vec<PreparedSegment>,
        pages: crate::PagedDatabase,
        expected: Option<&ReplicaHead>,
    ) -> Result<ReplicaHead> {
        let mut segments = expected
            .map(|h| h.manifest.segments.clone())
            .unwrap_or_default();
        for prepared in prepared {
            let segment = prepared.segment;
            if segment.bundle.is_none() {
                self.put(
                    &self.object_path(&segment.info.blake3, "ltx"),
                    Bytes::from(prepared.bytes),
                )
                .await?;
            }
            self.put(
                &self.object_path(&segment.index_hash, "idx"),
                Bytes::from(prepared.index),
            )
            .await?;
            segments.push(segment);
        }
        let mut head = self.publish(segments, pages.position(), expected).await?;
        head.pages = Some(pages);
        Ok(head)
    }
}
