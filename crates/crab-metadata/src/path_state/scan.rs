use super::*;

/// One verified record, node or edge emitted in dependency order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStateItem {
    Record {
        ordinal: u32,
        record: PathStateRecord,
    },
    Node {
        reference: PathStateNodeRef,
        value: Option<u32>,
    },
    Edge {
        parent: PathStateNodeRef,
        name: Arc<[u8]>,
        child: PathStateNodeRef,
    },
}

/// Exhaustive maintenance traversal retaining only the current layer's dictionary and page audit.
pub struct PathStateScan<'a> {
    reader: &'a PathStateReader,
    store: &'a Store,
    graph: &'a SplitCommitGraph,
    layer: usize,
    phase: Phase,
    names: Vec<Arc<[u8]>>,
    used: Vec<bool>,
    audit: BTreeMap<u64, (Kind, PageRef)>,
    nodes: Cursor,
    records: Cursor,
    children: Option<(PathStateNodeRef, Cursor)>,
}

enum Phase {
    Begin,
    Nodes,
    Records,
    Finish,
    Failed,
}

#[derive(Default)]
struct Cursor {
    pending: Vec<PageRef>,
    leaf: Option<Arc<Page>>,
    index: usize,
}

impl PathStateReader {
    /// Scan every layer through authenticated pages; errors invalidate the unfinished traversal.
    pub fn scan<'a>(
        &'a self,
        store: &'a Store,
        graph: &'a SplitCommitGraph,
    ) -> Result<PathStateScan<'a>> {
        if !matches_graph(&self.descriptor, CommitGraphView::Materialized(graph))
            || self.graph_layers != graph.descriptor.layers
        {
            return corrupt("path-state scan graph identity changed");
        }
        let bytes =
            self.descriptor
                .layers
                .iter()
                .try_fold(self.descriptor_bytes, |bytes, layer| {
                    bytes
                        .checked_add(layer.bytes)
                        .ok_or_else(|| corruption("scan byte count overflows"))
                })?;
        if bytes > self.max_bytes {
            return corrupt("path-state scan exceeds its byte limit");
        }
        Ok(PathStateScan {
            reader: self,
            store,
            graph,
            layer: 0,
            phase: Phase::Begin,
            names: Vec::new(),
            used: Vec::new(),
            audit: BTreeMap::new(),
            nodes: Cursor::default(),
            records: Cursor::default(),
            children: None,
        })
    }
}

impl PathStateScan<'_> {
    /// Advance the scan; only EOF establishes complete physical and semantic closure.
    pub async fn next(&mut self) -> Result<Option<PathStateItem>> {
        if matches!(self.phase, Phase::Failed) {
            return corrupt("path-state scan previously failed");
        }
        let result = self.advance().await;
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    async fn advance(&mut self) -> Result<Option<PathStateItem>> {
        while self.layer < self.reader.descriptor.layers.len() {
            let reference = &self.reader.descriptor.layers[self.layer];
            let path = &self.reader.paths[self.layer];
            if let Some((parent, cursor)) = self.children.as_mut() {
                if let Some((key, value)) = cursor
                    .row(
                        self.reader,
                        self.store,
                        self.layer,
                        Kind::Children,
                        &mut self.audit,
                    )
                    .await?
                {
                    let ordinal = integer(&key)? as usize;
                    let name = self
                        .names
                        .get(ordinal)
                        .ok_or_else(|| corruption("component ordinal is out of bounds"))?;
                    let child = codec::decode_child(&value, path.as_ref())?;
                    self.reader.validate_node(child, Some(*parent))?;
                    self.used[ordinal] = true;
                    return Ok(Some(PathStateItem::Edge {
                        parent: *parent,
                        name: name.clone(),
                        child,
                    }));
                }
                self.children = None;
            }
            match self.phase {
                Phase::Begin => {
                    // Verify the whole object without retaining it. Page reads
                    // then validate their own identities even if origin changes.
                    let (meta, _, mut stream) = self.store.get_stream(path, None).await?;
                    if meta.size != reference.bytes {
                        return corrupt("layer length mismatch");
                    }
                    let mut hasher = blake3::Hasher::new();
                    while let Some(bytes) = stream.next().await {
                        hasher.update(&bytes?);
                    }
                    if hasher.finalize().to_hex().as_str() != reference.hash {
                        return corrupt("layer content identity mismatch");
                    }
                    let mut ordered = BTreeMap::new();
                    if let Some(root) = &reference.pages.names {
                        let mut names = Cursor::new(root.clone());
                        while let Some((key, value)) = names
                            .row(
                                self.reader,
                                self.store,
                                self.layer,
                                Kind::Names,
                                &mut self.audit,
                            )
                            .await?
                        {
                            let (ordinal, name) = codec::decode_name(&key, &value, path.as_ref())?;
                            if ordered.insert(ordinal, Arc::<[u8]>::from(name)).is_some() {
                                return corrupt("duplicate component ordinal");
                            }
                        }
                    }
                    for (index, (ordinal, name)) in ordered.into_iter().enumerate() {
                        if ordinal as usize != index
                            || self
                                .names
                                .last()
                                .is_some_and(|previous| previous.as_ref() >= name.as_ref())
                        {
                            return corrupt("component ordinals are incomplete or unordered");
                        }
                        self.names.push(name);
                    }
                    self.used = vec![false; self.names.len()];
                    self.nodes = Cursor::new(reference.pages.nodes.clone());
                    self.records = Cursor::new(reference.pages.records.clone());
                    self.phase = Phase::Nodes;
                }
                Phase::Nodes => {
                    if let Some((key, value)) = self
                        .nodes
                        .row(
                            self.reader,
                            self.store,
                            self.layer,
                            Kind::Nodes,
                            &mut self.audit,
                        )
                        .await?
                    {
                        let node = PathStateNodeRef {
                            layer: self.layer as u32,
                            index: integer(&key)?,
                        };
                        self.reader.validate_node(node, None)?;
                        let (value, children) =
                            codec::decode_node(&value, reference.bytes, path.as_ref())?;
                        if value
                            .is_some_and(|ordinal| ordinal >= self.reader.descriptor.commit_count)
                        {
                            return corrupt("node attribution is out of bounds");
                        }
                        self.children = children.map(|root| (node, Cursor::new(root)));
                        return Ok(Some(PathStateItem::Node {
                            reference: node,
                            value,
                        }));
                    }
                    self.phase = Phase::Records;
                }
                Phase::Records => {
                    if let Some((key, value)) = self
                        .records
                        .row(
                            self.reader,
                            self.store,
                            self.layer,
                            Kind::Records,
                            &mut self.audit,
                        )
                        .await?
                    {
                        let ordinal = reference
                            .base_ordinal
                            .checked_add(integer(&key)?)
                            .ok_or_else(|| corruption("record ordinal overflows"))?;
                        let record = codec::decode_record(&value, path.as_ref())?;
                        let commit = self
                            .graph
                            .record(ordinal)
                            .ok_or_else(|| corruption("commit graph record is missing"))?;
                        if record.oid != commit.oid
                            || record.first_parent != commit.parents.first().copied()
                        {
                            return corrupt("path-state record does not match its commit ordinal");
                        }
                        self.reader.validate_node(record.root, None)?;
                        if record.root.layer as usize > self.layer {
                            return corrupt("record root points to a future layer");
                        }
                        return Ok(Some(PathStateItem::Record { ordinal, record }));
                    }
                    self.phase = Phase::Finish;
                }
                Phase::Finish => {
                    if self.used.contains(&false) {
                        return corrupt("unused path-state component");
                    }
                    let mut offset = 0u64;
                    for (start, (_, page)) in &self.audit {
                        if *start != offset {
                            return corrupt("layer pages overlap or leave unused bytes");
                        }
                        offset = offset
                            .checked_add(u64::from(page.bytes))
                            .ok_or_else(|| corruption("layer coverage overflows"))?;
                    }
                    if offset != reference.bytes {
                        return corrupt("layer contains trailing unreferenced bytes");
                    }
                    self.layer += 1;
                    self.names.clear();
                    self.used.clear();
                    self.audit.clear();
                    self.phase = Phase::Begin;
                }
                Phase::Failed => return corrupt("path-state scan previously failed"),
            }
        }
        Ok(None)
    }
}

impl Cursor {
    fn new(root: PageRef) -> Self {
        Self {
            pending: vec![root],
            ..Self::default()
        }
    }

    async fn row(
        &mut self,
        reader: &PathStateReader,
        store: &Store,
        layer: usize,
        kind: Kind,
        audit: &mut BTreeMap<u64, (Kind, PageRef)>,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(page) = &self.leaf {
                let Page::Leaf(rows) = page.as_ref() else {
                    return corrupt("scan leaf changed kind");
                };
                if let Some(row) = rows.get(self.index) {
                    self.index += 1;
                    return Ok(Some(row.clone()));
                }
                self.leaf = None;
            }
            let Some(reference) = self.pending.last().cloned() else {
                return Ok(None);
            };
            reference.validate(kind, reader.descriptor.layers[layer].bytes)?;
            if let Some((previous_kind, previous)) = audit.get(&reference.offset)
                && (*previous_kind != kind || *previous != reference)
            {
                return corrupt("page has conflicting references");
            }
            let page = reader.page(store, layer, &reference, kind).await?;
            // Keep the cursor unchanged across I/O. A dropped next() future
            // must not skip a page and later claim complete scan coverage.
            self.pending.pop();
            audit.insert(reference.offset, (kind, reference));
            match page.as_ref() {
                Page::Leaf(_) => {
                    self.leaf = Some(page);
                    self.index = 0;
                }
                Page::Branch(children) => self.pending.extend(children.iter().rev().cloned()),
            }
        }
    }
}

fn integer(key: &[u8]) -> Result<u32> {
    Ok(u32::from_be_bytes(
        key.try_into()
            .map_err(|_| corruption("invalid ordinal key"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;
    use crate::path_state::{DEFAULT_MAX_PATH_STATE_BYTES, PathStateLayer, PathStateNode};

    #[tokio::test]
    async fn cancelled_page_read_can_resume_without_skipping_records() {
        let (store, layout, graph, write) = fixture().await;
        let reader = PathStateReader::load(
            &store,
            &layout,
            &write.descriptor_hash,
            CommitGraphView::Materialized(&graph),
            DEFAULT_MAX_PATH_STATE_BYTES,
        )
        .await
        .unwrap();
        let throttled = Arc::new(object_store::throttle::ThrottledStore::new(
            store.inner().clone(),
            object_store::throttle::ThrottleConfig {
                wait_get_per_call: std::time::Duration::from_secs(60),
                ..Default::default()
            },
        ));
        let slow = Store::new(throttled.clone());
        let mut cursor = Cursor::new(reader.descriptor.layers[0].pages.records.clone());
        let mut audit = BTreeMap::new();
        {
            let pending = cursor.row(&reader, &slow, 0, Kind::Records, &mut audit);
            tokio::pin!(pending);
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
        }
        throttled.config_mut(|config| config.wait_get_per_call = std::time::Duration::ZERO);
        let row = cursor
            .row(&reader, &slow, 0, Kind::Records, &mut audit)
            .await
            .unwrap();
        assert_eq!(
            row.map(|(key, bytes)| (
                integer(&key).unwrap(),
                codec::decode_record(&bytes, "test").unwrap()
            )),
            Some((0, write.index.record(0).unwrap().clone()))
        );
    }

    #[tokio::test]
    async fn scan_reconstructs_the_full_index_in_dependency_order() {
        let (store, layout, graph, write) = fixture().await;
        let reader = PathStateReader::load(
            &store,
            &layout,
            &write.descriptor_hash,
            CommitGraphView::Materialized(&graph),
            DEFAULT_MAX_PATH_STATE_BYTES,
        )
        .await
        .unwrap();
        let mut scan = reader.scan(&store, &graph).unwrap();
        let mut layers = reader
            .descriptor
            .layers
            .iter()
            .map(|reference| PathStateLayer {
                base_ordinal: reference.base_ordinal,
                records: Vec::new(),
                nodes: Vec::new(),
            })
            .collect::<Vec<_>>();
        while let Some(item) = scan.next().await.unwrap() {
            match item {
                PathStateItem::Node { reference, value } => {
                    assert_eq!(
                        reference.index as usize,
                        layers[reference.layer as usize].nodes.len()
                    );
                    layers[reference.layer as usize].nodes.push(PathStateNode {
                        value,
                        children: BTreeMap::new(),
                    });
                }
                PathStateItem::Edge {
                    parent,
                    name,
                    child,
                } => {
                    assert!(
                        layers[child.layer as usize]
                            .nodes
                            .get(child.index as usize)
                            .is_some()
                    );
                    layers[parent.layer as usize].nodes[parent.index as usize]
                        .children
                        .insert(name, child);
                }
                PathStateItem::Record { ordinal, record } => {
                    let layer = layers.partition_point(|layer| layer.base_ordinal <= ordinal) - 1;
                    layers[layer].records.push(record);
                }
            }
        }
        assert_eq!(layers, write.index.layers());
    }

    #[tokio::test]
    async fn full_scan_rejects_unreferenced_bytes_and_cannot_resume_after_error() {
        let (store, layout, graph, write) = fixture().await;
        let mut descriptor = write.index.descriptor().clone();
        let layer = descriptor.layers.last_mut().unwrap();
        let mut bytes = store
            .get_with_etag_bounded(&layout.repo_path(&layer.path), layer.bytes)
            .await
            .unwrap()
            .0
            .to_vec();
        bytes.push(0);
        layer.hash = blake3::hash(&bytes).to_hex().to_string();
        layer.path = crate::path_state::path_state_layer_path(&layer.hash);
        layer.bytes = bytes.len() as u64;
        store
            .put(&layout.repo_path(&layer.path), bytes::Bytes::from(bytes))
            .await
            .unwrap();
        let bytes = serde_json::to_vec(&descriptor).unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        store
            .put(
                &layout.bulk_manifest_path("path-state", &hash),
                bytes::Bytes::from(bytes),
            )
            .await
            .unwrap();
        let reader = PathStateReader::load(
            &store,
            &layout,
            &hash,
            CommitGraphView::Materialized(&graph),
            DEFAULT_MAX_PATH_STATE_BYTES,
        )
        .await
        .unwrap();
        let mut scan = reader.scan(&store, &graph).unwrap();
        while matches!(scan.next().await, Ok(Some(_))) {}
        assert!(scan.next().await.is_err());
    }
}
