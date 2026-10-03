use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleRun, CapsuleTransaction,
};

#[test]
#[ignore = "synthetic compaction CPU measurement; not an end-to-end latency gate"]
fn compaction_cpu_measurement() {
    let leaves = (0..32)
        .map(|ordinal| {
            let transaction = CapsuleTransaction::for_protected_source(
                &format!("{ordinal:064x}"),
                &format!("{ordinal:064x}"),
                vec![CapsuleRefEdit::new(
                    "refs/heads/main",
                    None,
                    Some(format!("{ordinal:040x}")),
                    None,
                )],
            )
            .unwrap();
            let capsule = Capsule::build(
                &transaction,
                vec![
                    CapsuleGitPack::new(
                        Bytes::from(vec![ordinal as u8; 256 * 1024]),
                        Bytes::from_static(b"index"),
                        Bytes::from_static(b"reverse"),
                        Bytes::from_static(b"locator"),
                        "4".repeat(40),
                        1,
                    )
                    .unwrap(),
                ],
                Vec::new(),
            )
            .unwrap();
            CapsuleRun::leaf_with_member_oids(capsule, vec![vec![[ordinal as u8; 20]]]).unwrap()
        })
        .collect::<Vec<_>>();
    // Both algorithms consume the same immutable leaves. Comparing hashes
    // across fresh unplanned transactions would compare different UUIDs.
    let mut expected = None;
    for single_pass in [false, true] {
        let started = std::time::Instant::now();
        for _ in 0..5 {
            let run = if single_pass {
                CapsuleRun::compact(leaves.clone()).unwrap()
            } else {
                let mut runs = leaves.clone();
                while runs.len() > 1 {
                    runs = runs
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| CapsuleRun::compact(pair.to_vec()).unwrap())
                        .collect();
                }
                runs.pop().unwrap()
            };
            if let Some(expected) = &expected {
                assert_eq!(&run, expected);
            } else {
                expected = Some(run.clone());
            }
            std::hint::black_box(run);
        }
        eprintln!(
            "five 32-leaf compactions single_pass={single_pass}: {:?}",
            started.elapsed()
        );
    }
}
