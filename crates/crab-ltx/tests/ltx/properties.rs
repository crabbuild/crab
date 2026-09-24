//! Randomized capture, restore, and compaction properties.
//!
//! The restoration contract is that a verified plan reproduces exactly the
//! captured database image, and the repository invariant states it as
//! "reconstruction is byte-identical to original or returns an error". These
//! properties hold that over generated write shapes, so page sets, freelist
//! pages, overflow chains, and WAL cuts vary instead of matching one
//! hand-written fixture. Damage properties hold the other half: a torn segment
//! may never be accepted, however small the tear.

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

#[derive(Clone, Copy, Debug)]
enum Damage {
    /// Keeps only a prefix of the encoded segment, as a torn write would.
    Truncate { keep: u32 },
    /// Flips one byte while keeping the length intact.
    Flip { at: u32, mask: u8 },
}

impl Damage {
    fn apply(self, bytes: &[u8]) -> Vec<u8> {
        let length = bytes.len().max(1);
        match self {
            Damage::Truncate { keep } => bytes[..(keep as usize) % length].to_vec(),
            Damage::Flip { at, mask } => {
                let mut damaged = bytes.to_vec();
                if let Some(byte) = damaged.get_mut((at as usize) % length) {
                    *byte ^= mask | 1;
                }
                damaged
            }
        }
    }
}

fn damage_strategy() -> impl Strategy<Value = Damage> {
    prop_oneof![
        any::<u32>().prop_map(|keep| Damage::Truncate { keep }),
        (any::<u32>(), any::<u8>()).prop_map(|(at, mask)| Damage::Flip { at, mask }),
    ]
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

    #[test]
    fn torn_segment_bytes_are_never_accepted(damage in damage_strategy()) {
        let directory = tempfile::TempDir::new().unwrap();
        let source = directory.path().join("source.sqlite");
        let mut writer = Db::open(&source, Limits::default()).unwrap();
        writer
            .transaction(|transaction| {
                transaction.execute_batch(
                    "CREATE TABLE payload(id INTEGER PRIMARY KEY, data BLOB NOT NULL)",
                )?;
                transaction.execute("INSERT INTO payload(data) VALUES(randomblob(4096))", [])?;
                Ok(())
            })
            .unwrap();
        let batch = writer.capture().unwrap();
        writer.close().unwrap();
        let limits = Limits::default();
        // The pristine chain is the control: the harness must accept it.
        prop_assert!(VerifiedPlan::new(&batch.segments, batch.position, limits).is_ok());

        let victim = batch.segments.last().expect("capture produced a segment");
        let info = victim.info().clone();
        let bytes = fs::read(victim.path()).unwrap();
        let damaged = damage.apply(&bytes);
        let damaged_path = directory.path().join("damaged.ltx");
        fs::write(&damaged_path, &damaged).unwrap();
        let mut chain = batch.segments.clone();
        let last = chain.len() - 1;
        chain[last] = LocalSegment::new(damaged_path, info);

        prop_assert!(
            VerifiedPlan::new(&chain, batch.position, limits).is_err(),
            "a torn segment was accepted: {} of {} bytes",
            damaged.len(),
            bytes.len()
        );
    }
}
