use super::super::{
    DEFAULT_MAX_PATH_STATE_BYTES, PathStateInput, PathStateMutation, append_path_state,
    upload_path_state,
};
use super::*;
use bytes::Bytes;
use object_store::{ObjectStoreExt, memory::InMemory};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) async fn fixture() -> (
    Store,
    StoreLayout<Store>,
    SplitCommitGraph,
    super::super::PathStateWrite,
) {
    let graph = super::super::tests::graph_with_commits(2);
    let input = |n| PathStateInput {
        oid: [n; 20],
        first_parent: (n == 2).then_some([1; 20]),
        author: b"author".to_vec(),
        author_seconds: i64::from(n),
        message: vec![n],
        mutations: if n == 1 {
            (0..3000)
                .map(|index| PathStateMutation {
                    path: format!("tree/name-{index:04}").into_bytes(),
                    present: true,
                    reset: false,
                })
                .collect()
        } else {
            vec![PathStateMutation {
                path: b"tree/name-0001".to_vec(),
                present: true,
                reset: false,
            }]
        },
    };
    let base = append_path_state(None, &graph, vec![input(1)]).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "paged-attribution".to_owned());
    upload_path_state(&store, &layout, &base, DEFAULT_MAX_PATH_STATE_BYTES)
        .await
        .unwrap();
    let write = append_path_state(Some(base.into_index()), &graph, vec![input(2)]).unwrap();
    upload_path_state(&store, &layout, &write, DEFAULT_MAX_PATH_STATE_BYTES)
        .await
        .unwrap();
    (store, layout, graph, write)
}

#[tokio::test]
async fn batched_pages_preserve_old_roots_order_and_repeated_paths() {
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
    let paths = [
        b"tree/name-2999".as_slice(),
        b"tree/name-0001",
        b"tree/name-2999",
        b"tree/name-0000",
    ];
    for commit in [[1; 20], [2; 20]] {
        let actual = reader
            .latest(
                &store,
                CommitGraphView::Materialized(&graph),
                &commit,
                &paths,
            )
            .await
            .unwrap();
        let expected = write.index.latest(&graph, &commit, &paths).unwrap();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn query_reads_only_dependency_pages_and_caches_verified_results() {
    let (store, layout, graph, write) = fixture().await;
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = store.with_read_byte_observer(Arc::new(move |read| {
        observed.fetch_add(read, Ordering::Relaxed);
    }));
    let reader = PathStateReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        CommitGraphView::Materialized(&graph),
        DEFAULT_MAX_PATH_STATE_BYTES,
    )
    .await
    .unwrap();
    bytes.store(0, Ordering::Relaxed);
    let paths = [b"tree/name-0001".as_slice()];
    reader
        .latest(
            &store,
            CommitGraphView::Materialized(&graph),
            &[2; 20],
            &paths,
        )
        .await
        .unwrap();
    let cold = bytes.swap(0, Ordering::Relaxed);
    reader
        .latest(
            &store,
            CommitGraphView::Materialized(&graph),
            &[2; 20],
            &paths,
        )
        .await
        .unwrap();
    let full = write
        .index
        .descriptor
        .layers
        .iter()
        .map(|layer| layer.bytes)
        .sum::<u64>();
    assert_eq!((cold < full, bytes.load(Ordering::Relaxed)), (true, 0));
}

#[tokio::test]
async fn query_rejects_a_rebuilt_graph_with_matching_scalar_identity() {
    let (store, layout, mut graph, write) = fixture().await;
    let reader = PathStateReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        CommitGraphView::Materialized(&graph),
        DEFAULT_MAX_PATH_STATE_BYTES,
    )
    .await
    .unwrap();
    graph.descriptor.layers[0].hash = "f".repeat(64);
    assert!(
        reader
            .latest(
                &store,
                CommitGraphView::Materialized(&graph),
                &[2; 20],
                &[b"tree/name-0001"]
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn required_corrupt_or_missing_pages_never_become_a_lookup_miss() {
    let (store, layout, graph, write) = fixture().await;
    let reference = &write.index.descriptor.layers[1];
    let path = layout.repo_path(&reference.path);
    for corruption in [true, false] {
        if corruption {
            store
                .inner()
                .put(&path, Bytes::from(vec![0; reference.bytes as usize]).into())
                .await
                .unwrap();
        } else {
            store.delete(&path).await.unwrap();
        }
        let reader = PathStateReader::load(
            &store,
            &layout,
            &write.descriptor_hash,
            CommitGraphView::Materialized(&graph),
            DEFAULT_MAX_PATH_STATE_BYTES,
        )
        .await
        .unwrap();
        assert!(
            reader
                .latest(
                    &store,
                    CommitGraphView::Materialized(&graph),
                    &[2; 20],
                    &[b"tree/name-0001"]
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cache_identity_includes_the_page_kind() {
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
    let reference = &reader.descriptor.layers[1].pages.records;
    reader
        .page(&store, 1, reference, Kind::Records)
        .await
        .unwrap();
    assert!(
        reader
            .page(&store, 1, reference, Kind::Nodes)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn warm_cache_cannot_bypass_the_query_byte_limit() {
    let (store, layout, graph, write) = fixture().await;
    let mut reader = PathStateReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        CommitGraphView::Materialized(&graph),
        DEFAULT_MAX_PATH_STATE_BYTES,
    )
    .await
    .unwrap();
    reader
        .latest(
            &store,
            CommitGraphView::Materialized(&graph),
            &[2; 20],
            &[b"tree/name-0001"],
        )
        .await
        .unwrap();
    reader.max_bytes = reader.descriptor_bytes;
    assert!(
        reader
            .latest(
                &store,
                CommitGraphView::Materialized(&graph),
                &[2; 20],
                &[b"tree/name-0001"]
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn page_queries_remain_send_without_holding_cache_guards_across_io() {
    fn require_send<T: Send>(future: T) -> T {
        future
    }
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
    let result = require_send(reader.latest(
        &store,
        CommitGraphView::Materialized(&graph),
        &[2; 20],
        &[b"tree/name-0001"],
    ))
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn paged_graph_binding_matches_materialized_attribution_without_layer_loading() {
    use crate::split_commit_graph::{
        CommitGraphReader, append_paged_commit_graph, upload_split_commit_graph,
    };
    let (store, layout, graph, write) = fixture().await;
    let paged = append_paged_commit_graph(
        Some(graph.clone()),
        graph.descriptor.generation,
        graph.descriptor.pack_index_hash.clone(),
        graph.descriptor.git_validation_digest.clone(),
        &[[2; 20]],
        Vec::new(),
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &paged)
        .await
        .unwrap();
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store = store.with_read_byte_observer(Arc::new(move |read| {
        observed.fetch_add(read, Ordering::Relaxed);
    }));
    let graph_reader = CommitGraphReader::load(&store, &layout, &paged.descriptor_hash, 512 * 1024)
        .await
        .unwrap();
    let reader = PathStateReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        CommitGraphView::Paged(&graph_reader),
        DEFAULT_MAX_PATH_STATE_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(
        bytes.load(Ordering::Relaxed),
        paged.descriptor_bytes.len() as u64 + reader.descriptor_bytes
    );
    let paths = [
        b"tree/name-0001".as_slice(),
        b"tree/name-2999",
        b"tree/name-0001",
    ];
    let expected = write.index.latest(&graph, &[2; 20], &paths).unwrap();
    assert_eq!(
        reader
            .latest(
                &store,
                CommitGraphView::Paged(&graph_reader),
                &[2; 20],
                &paths
            )
            .await
            .unwrap(),
        expected
    );
}
