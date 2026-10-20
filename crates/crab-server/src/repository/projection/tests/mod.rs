use super::*;

#[test]
fn attribution_batches_respect_the_query_wire_budget() {
    let paths = (0..200)
        .map(|index| format!("dir/{index:03}/{}", "x".repeat(700)).into_bytes())
        .collect::<Vec<_>>();
    let batches = attribution_batches(&"a".repeat(64), &[0x11; 20], paths.clone()).unwrap();
    let flattened = batches.iter().flatten().cloned().collect::<Vec<_>>();
    assert_eq!(flattened, paths);
    assert!(batches.iter().all(|batch| {
        serde_json::to_vec(&projection::AttributionQuery {
            source_token: "a".repeat(64),
            commit_oid: vec![0x11; 20],
            paths: batch.clone(),
        })
        .unwrap()
        .len()
            <= MAX_ATTRIBUTION_QUERY_BYTES
    }));
}

#[test]
fn attribution_batches_reject_one_oversized_path() {
    let paths = vec![vec![b'x'; MAX_ATTRIBUTION_QUERY_BYTES]];
    assert!(attribution_batches("token", &[0x11; 20], paths).is_err());
}

mod probes;
