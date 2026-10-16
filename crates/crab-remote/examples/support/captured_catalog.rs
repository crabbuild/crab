//! Keep a canonical catalog session cold across externally driven compaction and GC.
use std::io::{self, Write};

use crab_metadata::file_index_lookup::FileIndexLookupSession;
use crab_remote_git::GitPath;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use serde_json::json;

use super::support::{Measurements, Result};

pub async fn qualify(
    layout: &StoreLayout<Store>,
    key: &GitPath,
    counters: &Measurements,
) -> Result<()> {
    let key = std::str::from_utf8(key.as_bytes())?;
    let hash = MerkleHash::from_hex(key)?;
    let session = counters
        .measure("open", "cold", async {
            Ok(
                FileIndexLookupSession::open_for_storage(layout.store(), layout.repo_prefix())
                    .await?,
            )
        })
        .await?;
    let result: Result<_> = async {
        // Capturing the current-state session must not prefetch its selected
        // file pages or require browse maintenance to establish their lifetime.
        println!("{}", json!({"kind":"captured", "schema":1, "file":key}));
        io::stdout().flush()?;
        let command = tokio::task::spawn_blocking(|| -> io::Result<String> {
            let mut command = String::new();
            io::stdin().read_line(&mut command)?;
            Ok(command)
        })
        .await??;
        if command.trim() != "continue" {
            return Err(io::Error::other(
                "catalog capture requires a continue command after external GC",
            )
            .into());
        }
        let mut previous = None;
        for cache in ["cold", "warm"] {
            let selected = counters
                .measure("catalog-current", cache, async {
                    Ok(session.lookup_batch(&[hash, hash]).await?)
                })
                .await?;
            if previous.as_ref().is_some_and(|value| value != &selected) {
                return Err(io::Error::other("captured cold/warm catalog results differ").into());
            }
            println!("{}", json!({"kind":"captured-result", "operation":"catalog-current", "cache":cache,
                "shards":selected.iter().map(|hash| hash.as_ref().map(MerkleHash::hex)).collect::<Vec<_>>()}));
            previous = Some(selected);
        }
        Ok(previous)
    }
    .await;
    // EOF, failed catalog reads and oracle mismatches must all drain the same
    // session. The bulk oracle runs only after close, outside measured reads.
    let closed = session.close().await;
    let selected = result?;
    closed?;
    let full = crab_metadata::capsule_protocol::load_pointer_catalog(layout).await?;
    let expected = full.files().get(&hash.hex()).ok_or_else(|| {
        io::Error::other("captured catalog file is absent from maintenance oracle")
    })?;
    let expected = MerkleHash::from_hex(expected.shard_hash())?;
    if selected != Some(vec![Some(expected), Some(expected)]) {
        return Err(io::Error::other("captured catalog differs from maintenance oracle").into());
    }
    Ok(())
}
