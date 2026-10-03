use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{
    MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES, MAX_RECONCILIATION_MAPPING_ENTRIES,
    SOURCES_PER_RECONCILIATION_BATCH, build_mapping, inspect_mapping, load_xorb, parse_hash,
    source_chunks,
};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::optimize::xorbs::journal::{OptimizeXorbsJournal, RunRow, SCHEMA_VERSION, SourceStatus};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_xet::xorb::format::MerkleHash;

/// Upgrade a released per-row journal before executing its remaining sources.
pub(crate) async fn prepare_resume(
    journal: &OptimizeXorbsJournal,
    run: &RunRow,
    store: &Store,
    router: &StoreLayout,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    match run.schema_ver {
        SCHEMA_VERSION => return Ok(()),
        1 => {}
        version => {
            return Err(CrabError::Configuration {
                key: "xorb optimization journal version".to_owned(),
                origin: format!("unsupported journal version {version}"),
            });
        }
    }
    let counts = journal.count_by_status(&run.run_id)?;
    if counts.total() > MAX_RECONCILIATION_MAPPING_ENTRIES || counts.staged != 0 {
        return Err(CrabError::Configuration {
            key: "xorb optimization journal migration".to_owned(),
            origin: "legacy run exceeds the bounded source count or has unsupported staged sources"
                .to_owned(),
        });
    }
    let (mapping, _, _) = build_mapping(journal, &run.run_id)?;
    if counts.done != mapping.len() as u64 {
        return Err(CrabError::CorruptObject {
            path: journal.path().display().to_string(),
            reason: "legacy completed sources lack destination mappings".to_owned(),
        });
    }
    let (loaded, coverage) = inspect_mapping(store, router, &mapping, cancel).await?;
    let mut incomplete = VecDeque::new();
    let mut missing = HashSet::<(MerkleHash, u32)>::new();
    for (destination, covered) in coverage {
        let info = loaded.destination_infos.get(&destination).ok_or_else(|| {
            CrabError::Internal("verified destination coverage has no xorb metadata".to_owned())
        })?;
        if covered.iter().any(|covered| !covered) {
            incomplete.push_back(destination);
        }
        for (chunk, covered) in info.chunks.iter().zip(covered) {
            if !covered {
                missing.insert((chunk.chunk_hash, chunk.unpacked_segment_bytes));
            }
        }
    }
    // v1.2.4 completed source rows independently after uploading a shared
    // destination. A chunk outside the mapped closure is repairable only if
    // a verified original source recorded in this same run owns it.
    let mut chunk_entries = loaded
        .sources
        .values()
        .map(|source| source.chunks.len())
        .chain(
            loaded
                .destination_infos
                .values()
                .map(|info| info.chunks.len()),
        )
        .sum::<usize>();
    for source in loaded.sources.values() {
        for chunk in &source.chunks {
            missing.remove(&(chunk.hash, chunk.size));
        }
    }
    let mut after = String::new();
    while !missing.is_empty() {
        check_cancelled(cancel)?;
        let pending = journal.sources_by_status_after(
            &run.run_id,
            SourceStatus::Pending,
            Some(&after),
            SOURCES_PER_RECONCILIATION_BATCH,
        )?;
        if pending.is_empty() {
            return Err(CrabError::CorruptObject {
                path: journal.path().display().to_string(),
                reason:
                    "incomplete destination has chunks outside the verified sources of this run"
                        .to_owned(),
            });
        }
        for source in pending {
            check_cancelled(cancel)?;
            let hash = parse_hash(&source.src_xorb, "xorb optimization journal source")?;
            let path = router.xorb_path(&hash).to_string();
            let (parser, _) = load_xorb(store, router, hash).await?;
            let chunks = source_chunks(&parser, &path)?;
            chunk_entries = chunk_entries.checked_add(chunks.len()).ok_or_else(|| {
                CrabError::Configuration {
                    key: "xorb optimization journal migration chunk metadata".to_owned(),
                    origin: "source chunk metadata count overflows usize".to_owned(),
                }
            })?;
            if chunk_entries > MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES {
                return Err(CrabError::Configuration {
                    key: "xorb optimization journal migration chunk metadata".to_owned(),
                    origin: format!(
                        "verified chunk metadata exceeds {MAX_RECONCILIATION_LOADED_CHUNK_ENTRIES} entries"
                    ),
                });
            }
            for chunk in chunks {
                missing.remove(&(chunk.hash, chunk.size));
            }
            after = source.src_xorb;
            if missing.is_empty() {
                break;
            }
        }
    }
    // Requeue the connected component, not the whole run: otherwise another
    // source sharing a complete destination could be stranded by this repair.
    // Removing visited destinations bounds traversal by journal link count.
    let mut sources_by_destination = HashMap::<MerkleHash, Vec<&str>>::new();
    for (source, destinations) in &mapping {
        for destination in destinations {
            let hash = parse_hash(destination, "xorb optimization journal destination")?;
            sources_by_destination.entry(hash).or_default().push(source);
        }
    }
    let mut requeue = BTreeSet::new();
    while let Some(destination) = incomplete.pop_front() {
        check_cancelled(cancel)?;
        if let Some(sources) = sources_by_destination.remove(&destination) {
            for source in sources {
                if requeue.insert(source.to_owned()) {
                    let destinations = mapping.get(source).ok_or_else(|| {
                        CrabError::Internal("verified source has no journal mapping".to_owned())
                    })?;
                    for destination in destinations {
                        incomplete.push_back(parse_hash(
                            destination,
                            "xorb optimization journal destination",
                        )?);
                    }
                }
            }
        }
    }
    let requeue = requeue.into_iter().collect::<Vec<_>>();
    check_cancelled(cancel)?;
    journal.upgrade_legacy_run(&run.run_id, &requeue)?;
    info!(run_id = %run.run_id, sources_requeued = requeue.len(), "upgraded xorb optimization journal");
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;
    use crate::optimize::xorbs::profile::Profile;
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    async fn put_xorb(store: &Store, router: &StoreLayout, chunks: &[Chunk]) -> String {
        let mut builder = XorbBuilder::new();
        for chunk in chunks {
            builder.push(chunk, RunId(0)).unwrap();
        }
        let xorb = builder.finalize().unwrap().remove(0);
        store
            .put(&router.xorb_path(&xorb.hash), Bytes::from(xorb.bytes))
            .await
            .unwrap();
        xorb.hash.hex()
    }

    fn old_journal(
        path: &std::path::Path,
        mappings: &[(String, Option<String>)],
    ) -> OptimizeXorbsJournal {
        {
            let journal = OptimizeXorbsJournal::open(path).unwrap();
            journal
                .start_run("migration", &Profile::code().to_json())
                .unwrap();
            for (source, destination) in mappings {
                journal.insert_source("migration", source).unwrap();
                if let Some(destination) = destination {
                    journal
                        .update_source_status(
                            "migration",
                            source,
                            SourceStatus::Done,
                            Some(destination),
                        )
                        .unwrap();
                }
            }
        }
        {
            let connection = rusqlite::Connection::open(path).unwrap();
            connection
                .execute("UPDATE runs SET schema_ver = 1", [])
                .unwrap();
        }
        OptimizeXorbsJournal::open(path).unwrap()
    }

    #[tokio::test]
    async fn legacy_migration_rejects_foreign_or_corrupt_body_without_changing_progress() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/migration-rejection".to_owned());
        let a = Chunk::new(Bytes::from(vec![31; 1024]));
        let b = Chunk::new(Bytes::from(vec![32; 1024]));
        let foreign = Chunk::new(Bytes::from(vec![33; 1024]));
        let source_a = put_xorb(&store, &router, std::slice::from_ref(&a)).await;
        let source_b = put_xorb(&store, &router, std::slice::from_ref(&b)).await;
        // Merely existing in the store cannot make this chunk part of the run.
        put_xorb(&store, &router, std::slice::from_ref(&foreign)).await;
        let destination = put_xorb(&store, &router, &[a, foreign]).await;
        for corrupt in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let journal = old_journal(
                &directory.path().join("journal.db"),
                &[
                    (
                        source_a.clone(),
                        Some(serde_json::to_string(&[&destination]).unwrap()),
                    ),
                    (source_b.clone(), None),
                ],
            );
            if corrupt {
                let hash = MerkleHash::from_hex(&destination).unwrap();
                // Simulate stored corruption explicitly; immutable put would
                // reject this replacement before migration reads the body.
                store
                    .put_overwrite(
                        &router.xorb_path(&hash),
                        Bytes::from_static(b"corrupt body"),
                    )
                    .await
                    .unwrap();
            }
            let run = journal.active_run().unwrap().unwrap();
            assert!(
                prepare_resume(&journal, &run, &store, &router, &CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(journal.active_run().unwrap().unwrap().schema_ver, 1);
            let counts = journal.count_by_status(&run.run_id).unwrap();
            assert_eq!((counts.done, counts.pending), (1, 1));
            let (mapping, _, _) = build_mapping(&journal, &run.run_id).unwrap();
            assert_eq!(mapping[&source_a], vec![destination.clone()]);
        }
    }

    #[tokio::test]
    async fn legacy_migration_requeues_entire_destination_component_only() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/migration-components".to_owned());
        let chunks = (41..=47)
            .map(|value| Chunk::new(Bytes::from(vec![value; 1024])))
            .collect::<Vec<_>>();
        let sources = [
            put_xorb(&store, &router, &chunks[0..1]).await,
            put_xorb(&store, &router, &[chunks[1].clone(), chunks[3].clone()]).await,
            put_xorb(&store, &router, &chunks[2..3]).await,
            put_xorb(&store, &router, &chunks[4..5]).await,
            put_xorb(&store, &router, &chunks[5..7]).await,
        ];
        let incomplete = put_xorb(&store, &router, &chunks[0..3]).await;
        let complete = put_xorb(&store, &router, &chunks[3..5]).await;
        let unrelated = put_xorb(&store, &router, &[chunks[6].clone(), chunks[5].clone()]).await;
        let directory = tempfile::tempdir().unwrap();
        let journal = old_journal(
            &directory.path().join("journal.db"),
            &[
                (
                    sources[0].clone(),
                    Some(serde_json::to_string(&[&incomplete]).unwrap()),
                ),
                (
                    sources[1].clone(),
                    Some(serde_json::to_string(&[&incomplete, &complete]).unwrap()),
                ),
                (sources[2].clone(), None),
                (
                    sources[3].clone(),
                    Some(serde_json::to_string(&[&complete]).unwrap()),
                ),
                (
                    sources[4].clone(),
                    Some(serde_json::to_string(&[&unrelated]).unwrap()),
                ),
            ],
        );
        let run = journal.active_run().unwrap().unwrap();
        prepare_resume(&journal, &run, &store, &router, &CancellationToken::new())
            .await
            .unwrap();
        let done = journal
            .sources_by_status(&run.run_id, SourceStatus::Done)
            .unwrap();
        assert_eq!(
            done.iter()
                .map(|row| row.src_xorb.as_str())
                .collect::<Vec<_>>(),
            vec![sources[4].as_str()]
        );
        let pending = journal
            .sources_by_status(&run.run_id, SourceStatus::Pending)
            .unwrap();
        assert_eq!(
            pending
                .into_iter()
                .map(|row| row.src_xorb)
                .collect::<BTreeSet<_>>(),
            sources[0..4].iter().cloned().collect()
        );
    }

    #[tokio::test]
    async fn cancelled_or_unknown_version_resume_does_not_migrate() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/migration-version".to_owned());
        for version in [1, 99] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("journal.db");
            drop(old_journal(&path, &[]));
            {
                let connection = rusqlite::Connection::open(&path).unwrap();
                connection
                    .execute("UPDATE runs SET schema_ver = ?1", [version])
                    .unwrap();
            }
            let journal = OptimizeXorbsJournal::open(&path).unwrap();
            let run = journal.active_run().unwrap().unwrap();
            let cancel = CancellationToken::new();
            if version == 1 {
                cancel.cancel();
            }
            assert!(
                prepare_resume(&journal, &run, &store, &router, &cancel)
                    .await
                    .is_err()
            );
            assert_eq!(
                journal.active_run().unwrap().unwrap().schema_ver,
                version as u32
            );
        }
    }

    #[tokio::test]
    async fn atomic_version_resume_does_not_read_remote_bodies() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/migration-fast-path".to_owned());
        let directory = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&directory.path().join("journal.db")).unwrap();
        journal
            .start_run("atomic", &Profile::code().to_json())
            .unwrap();
        // Neither body exists. Inspection would fail, so this proves the new
        // version's resume does not incur legacy migration reads.
        journal
            .insert_source("atomic", &MerkleHash::from([51; 32]).hex())
            .unwrap();
        journal
            .update_source_status(
                "atomic",
                &MerkleHash::from([51; 32]).hex(),
                SourceStatus::Done,
                Some(&serde_json::to_string(&[MerkleHash::from([52; 32]).hex()]).unwrap()),
            )
            .unwrap();
        let run = journal.active_run().unwrap().unwrap();
        prepare_resume(&journal, &run, &store, &router, &CancellationToken::new())
            .await
            .unwrap();
    }
}
