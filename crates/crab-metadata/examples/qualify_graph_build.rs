//! Measure paged graph construction on the frozen ancestry-growth DAG shapes.
use std::{error::Error, io, time::Instant};

use crab_metadata::split_commit_graph::{CommitGraphInput, append_paged_commit_graph};
use serde_json::json;

fn oid(index: u32) -> [u8; 20] {
    let mut value = [0; 20];
    value[16..].copy_from_slice(&index.to_be_bytes());
    value
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [shape, count] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: qualify_graph_build <linear|branches|criss-cross|disconnected> <commits>",
        )
        .into());
    };
    let count = count.parse::<u32>()?;
    if count == 0 || count > 100_000 {
        return Err(io::Error::other("commit count must be between 1 and 100000").into());
    }
    if !matches!(
        shape.as_str(),
        "linear" | "branches" | "criss-cross" | "disconnected"
    ) {
        return Err(io::Error::other("unknown graph shape").into());
    }
    // Synthetic OIDs isolate builder cost; these are not Git object-integrity
    // receipts. Parent layouts match the ancestry storage-growth regression.
    let inputs = (0..count)
        .map(|index| {
            let parents = match shape.as_str() {
                "linear" if index > 0 => vec![index - 1],
                "branches" if index >= 8 => vec![index - 8],
                "criss-cross" if index >= 2 => {
                    vec![(index / 2 - 1) * 2, (index / 2 - 1) * 2 + 1]
                }
                _ => Vec::new(),
            };
            CommitGraphInput {
                oid: oid(index),
                tree_oid: [0; 20],
                commit_time: 0,
                parents: parents.into_iter().map(oid).collect(),
            }
        })
        .collect::<Vec<_>>();
    let roots = inputs.iter().map(|input| input.oid).collect::<Vec<_>>();
    let pack_index_hash = "1".repeat(64);
    let validation_digest = "2".repeat(64);
    let started = Instant::now();
    let write =
        append_paged_commit_graph(None, 1, pack_index_hash, validation_digest, &roots, inputs)?
            .ok_or_else(|| io::Error::other("complete fixture graph was not produced"))?;
    let elapsed = started.elapsed();
    let bytes = write.descriptor_bytes.len() as u64
        + write
            .layers
            .iter()
            .map(|layer| layer.bytes.len() as u64)
            .sum::<u64>();
    let nodes = write.layers.iter().try_fold(0_u64, |total, layer| {
        let pages = layer
            .reference
            .pages
            .as_ref()
            .ok_or_else(|| io::Error::other("fixture graph lacks paged roots"))?;
        Ok::<_, io::Error>(total + u64::from(pages.ancestry_nodes.count))
    })?;
    println!(
        "{}",
        json!({"schema":1, "shape":shape, "commits":count,
        "build_elapsed_us":elapsed.as_micros(), "index_bytes":bytes,
        "ancestry_nodes":nodes, "descriptor_hash":write.descriptor_hash})
    );
    Ok(())
}
