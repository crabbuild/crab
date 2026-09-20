use crab_cell_runtime::{
    Digest, QualificationProfile, QualificationReceipt, QualificationRunner, QualificationWorkload,
};
use ed25519_dalek::SigningKey;

const SOURCE: &str = "qualification-source";
const IMAGE: Digest = Digest::from_bytes([7; 32]);
const MATRIX_ROWS: [&str; 10] = [
    "protocol",
    "storage",
    "publication",
    "warm-path",
    "churn",
    "fleet",
    "failover",
    "primitives",
    "accounting",
    "compatibility",
];

fn receipt(workload: &str, artifact: &[u8]) -> QualificationReceipt {
    QualificationRunner::new(SigningKey::from_bytes(&[11; 32]))
        .emit_with_profile(
            &QualificationProfile::pr_contract(),
            SOURCE.into(),
            IMAGE,
            "local".into(),
            workload.into(),
            "none".into(),
            Vec::new(),
            artifact,
            true,
            (
                "rustc".into(),
                "test".into(),
                "local".into(),
                0,
                0,
                0,
                false,
            ),
        )
        .expect("fixture receipt")
}

#[test]
fn workload_identity_is_reproducible_and_seed_bound() {
    let profile = QualificationProfile::pr_contract();
    let first = QualificationWorkload::generate(&profile, 17).expect("workload generation");
    let same = QualificationWorkload::generate(&profile, 17).expect("same workload generation");
    let changed =
        QualificationWorkload::generate(&profile, 18).expect("changed workload generation");

    assert_eq!(first, same);
    assert_eq!(first.outcome_digest(), same.outcome_digest());
    assert_ne!(first.outcome_digest(), changed.outcome_digest());
    first
        .verify_for_profile(&profile)
        .expect("canonical workload identity");
    assert_eq!(
        QualificationWorkload::decode(&first.encode().expect("workload encoding"))
            .expect("workload decoding"),
        first
    );
}

#[test]
fn public_matrix_verifier_rejects_missing_duplicate_and_forged_evidence() {
    let artifacts = MATRIX_ROWS
        .iter()
        .map(|row| format!("artifact-{row}").into_bytes())
        .collect::<Vec<_>>();
    let receipts = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, row)| receipt(row, &artifacts[index]))
        .collect::<Vec<_>>();
    let artifact_views = artifacts
        .iter()
        .map(|artifact| vec![artifact.as_slice()])
        .collect::<Vec<_>>();
    let evidence = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, row)| (*row, &receipts[index], artifact_views[index].as_slice()))
        .collect::<Vec<_>>();

    QualificationReceipt::verify_matrix(SOURCE, IMAGE, &evidence).expect("complete matrix");

    let mut missing = evidence.clone();
    missing.pop();
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &missing).is_err());

    let mut duplicate = evidence.clone();
    duplicate[1].0 = MATRIX_ROWS[0];
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &duplicate).is_err());

    let mut forged_artifact = artifacts[0].clone();
    forged_artifact.push(b'!');
    let forged_view = [forged_artifact.as_slice()];
    let mut forged = evidence;
    forged[0].2 = &forged_view;
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &forged).is_err());
}

#[test]
fn public_receipt_verifier_binds_source_image_and_artifact() {
    let artifact = b"raw-evidence";
    let receipt = receipt("protocol", artifact);
    receipt
        .verify_for(SOURCE, IMAGE, artifact)
        .expect("exact release identity");
    assert!(receipt.verify_for("other-source", IMAGE, artifact).is_err());
    assert!(
        receipt
            .verify_for(SOURCE, Digest::from_bytes([8; 32]), artifact)
            .is_err()
    );
    assert!(
        receipt
            .verify_for(SOURCE, IMAGE, b"forged-evidence")
            .is_err()
    );
    assert!(
        receipt
            .verify_for_trusted_signer(SOURCE, IMAGE, artifact, [12; 32])
            .is_err()
    );
}
