//! Randomized capture, restore, and compaction properties.
//!
//! The restoration contract is that a verified plan reproduces exactly the
//! captured database image, and the repository invariant states it as
//! "reconstruction is byte-identical to original or returns an error". These
//! properties hold that over generated write shapes, so page sets, freelist
//! pages, overflow chains, and WAL cuts vary instead of matching one
//! hand-written fixture.

use std::fs;

use crab_ltx::rusqlite::{Result as SqlResult, Transaction};
use crab_ltx::{Db, Limits, LocalSegment, Position, VerifiedPlan, compact_exact, restore_exact};
use proptest::prelude::*;

#[derive(Clone, Copy, Debug)]
struct Step {
    rows: u8,
    blob_bytes: u32,
    delete_rows: bool,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        (1_u8..=4, 0_u32..=32 * 1024, any::<bool>()).prop_map(|(rows, blob_bytes, delete_rows)| {
            Step {
                rows,
                blob_bytes,
                delete_rows,
            }
        }),
        1..4,
    )
}

fn apply(transaction: &Transaction<'_>, step: Step) -> SqlResult<()> {
    for _ in 0..step.rows {
        transaction.execute(
            "INSERT INTO payload(data) VALUES(randomblob(?1))",
            [i64::from(step.blob_bytes)],
        )?;
    }
    if step.delete_rows {
        transaction.execute("DELETE FROM payload WHERE id % 3 = 0", [])?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    #[test]
    fn capture_restore_and_compaction_reproduce_the_captured_image(steps in steps()) {
        let directory = tempfile::TempDir::new().unwrap();
        let source = directory.path().join("source.sqlite");
        let mut writer = Db::open(&source, Limits::default()).unwrap();
        writer
            .transaction(|transaction| {
                transaction.execute_batch(
                    "CREATE TABLE payload(id INTEGER PRIMARY KEY, data BLOB NOT NULL)",
                )
            })
            .unwrap();

        // Keep the exact chain behind each cut, the way a published root carries
        // the segments up to its captured position.
        let mut cuts: Vec<(Vec<LocalSegment>, Position)> = Vec::new();
        let mut chain: Vec<LocalSegment> = Vec::new();
        for step in steps {
            writer
                .transaction(|transaction| apply(transaction, step))
                .unwrap();
            let batch = writer.capture().unwrap();
            chain.extend(batch.segments.iter().cloned());
            cuts.push((chain.clone(), batch.position));
        }
        writer.close().unwrap();

        let last_position = cuts.last().expect("at least one cut").1;
        for (segments, position) in cuts {
            let plan = VerifiedPlan::new(&segments, position, Limits::default()).unwrap();
            let direct = directory
                .path()
                .join(format!("direct-{}.sqlite", position.txid));
            let repeat = directory
                .path()
                .join(format!("repeat-{}.sqlite", position.txid));
            restore_exact(&plan, &direct).unwrap();
            restore_exact(&plan, &repeat).unwrap();
            prop_assert_eq!(
                fs::read(&direct).unwrap(),
                fs::read(&repeat).unwrap(),
                "two restores of one plan differ"
            );

            let compacted = compact_exact(
                &plan,
                &directory.path().join(format!("snapshot-{}.ltx", position.txid)),
            )
            .unwrap();
            let compact_plan =
                VerifiedPlan::new(&[compacted], position, Limits::default()).unwrap();
            let from_compaction = directory
                .path()
                .join(format!("compact-{}.sqlite", position.txid));
            restore_exact(&compact_plan, &from_compaction).unwrap();
            prop_assert_eq!(
                fs::read(&direct).unwrap(),
                fs::read(&from_compaction).unwrap(),
                "compaction changed the restored image"
            );

            // The last cut is the source database as it stands after close, so
            // its restored image must be the original file byte for byte.
            if position == last_position {
                prop_assert_eq!(
                    fs::read(&direct).unwrap(),
                    fs::read(&source).unwrap(),
                    "restored image differs from the source database"
                );
            }
        }
    }
}
