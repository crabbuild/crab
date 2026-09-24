//! Private control transition and codec behavior.

use super::*;

fn owner(byte: u8) -> Owner {
    Owner {
        session: SessionId::from_bytes([byte; 16]),
        endpoint: format!("https://node-{byte}.internal:8081"),
    }
}

fn root(sequence: u64) -> RootRef {
    RootRef {
        digest: Digest::from_bytes([9; 32]),
        txid: sequence,
        checksum: CHECKSUM_FLAG | sequence,
        commit_sequence: sequence,
    }
}

fn recovery(predecessor: RootRef) -> RecoveryOverlayRef {
    RecoveryOverlayRef {
        leader_session: SessionId::from_bytes([3; 16]),
        log_epoch: 4,
        manifest_digest: Digest::from_bytes([7; 32]),
        first_node_sequence: 10,
        last_node_sequence: 12,
        predecessor,
        final_txid: 9,
        final_checksum: CHECKSUM_FLAG | 9,
        final_commit_sequence: 9,
    }
}

fn initial() -> Control {
    Control::initial(
        CellId::from_bytes([1; 32]),
        IncarnationId::from_bytes([2; 16]),
        owner(3),
        Digest::from_bytes([4; 32]),
        1,
    )
    .unwrap()
}

#[test]
fn canonical_control_roundtrips_and_rejects_alternate_encodings() {
    let mut control = initial();
    control.root = Some(root(7));
    control.state = ControlState::Serving;
    let bytes = control.encode().unwrap();
    assert_eq!(Control::decode(&bytes).unwrap(), control);

    let text = String::from_utf8(bytes).unwrap();
    assert!(
        Control::decode(
            text.replace("\"epoch\":\"1\"", "\"epoch\":\"01\"")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        Control::decode(
            text.replace("\"schema\":1", "\"schema\":1,\"schema\":1")
                .as_bytes()
        )
        .is_err()
    );
    assert!(Control::decode(format!("{{\"unknown\":1,{}}}", &text[1..]).as_bytes()).is_err());
}

#[test]
fn transitions_protect_scope_owner_and_published_root() {
    let recovering = initial();
    let mut serving = recovering.clone();
    serving.state = ControlState::Serving;
    serving.root = Some(root(1));
    serving.revision += 1;
    serving.progress += 1;
    recovering
        .validate_transition(&serving, Transition::Publish)
        .unwrap();
    let mut unlabelled_migration = serving.clone();
    unlabelled_migration.schema = 2;
    unlabelled_migration.root = Some(root(2));
    unlabelled_migration.revision += 1;
    unlabelled_migration.progress += 1;
    assert!(
        serving
            .validate_transition(&unlabelled_migration, Transition::Publish)
            .is_err()
    );
    serving
        .validate_transition(&unlabelled_migration, Transition::Migrate)
        .unwrap();
    let mut code_only = serving.clone();
    code_only.code = Digest::from_bytes([8; 32]);
    code_only.root = Some(root(2));
    code_only.revision += 1;
    code_only.progress += 1;
    serving
        .validate_transition(&code_only, Transition::Migrate)
        .unwrap();

    let renewed = serving.renew().unwrap();
    assert!(serving.is_same_or_pure_renewal_of(&serving));
    assert!(renewed.is_same_or_pure_renewal_of(&serving));
    let mut skipped_progress = renewed.clone();
    skipped_progress.revision += 1;
    assert!(!skipped_progress.is_same_or_pure_renewal_of(&serving));

    let takeover = renewed.takeover(owner(5)).unwrap();

    let activated = takeover.activate().unwrap();
    assert_eq!(activated.state, ControlState::Serving);
    assert_eq!(activated.root, takeover.root);
    assert!(takeover.activate().is_ok());
    assert!(activated.activate().is_err());

    let rootless = initial();
    assert!(rootless.activate().is_err());

    let mut corrupted_compaction = takeover.clone();
    corrupted_compaction.state = ControlState::Serving;
    corrupted_compaction.root.as_mut().unwrap().checksum ^= 1;
    corrupted_compaction.revision += 1;
    corrupted_compaction.progress += 1;
    assert!(
        takeover
            .validate_transition(&corrupted_compaction, Transition::Publish)
            .is_err()
    );
}

#[test]
fn recovery_attachment_is_canonical_and_survives_takeover() {
    let mut serving = initial();
    serving.state = ControlState::Serving;
    serving.root = Some(root(4));
    let attached = serving
        .attach_recovery(recovery(serving.root.clone().unwrap()))
        .unwrap();
    assert_eq!(
        Control::decode(&attached.encode().unwrap()).unwrap(),
        attached
    );
    let mut normal_publish = attached.clone();
    normal_publish.revision += 1;
    normal_publish.progress += 1;
    normal_publish.root = Some(root(9));
    assert!(
        attached
            .validate_transition(&normal_publish, Transition::Publish)
            .is_err()
    );

    let takeover = attached.takeover(owner(8)).unwrap();
    assert_eq!(takeover.state, ControlState::Recovering);
    assert_eq!(takeover.recovery, attached.recovery);
    assert!(takeover.activate().is_err());

    let mut changed = attached.clone();
    changed.revision += 1;
    changed.progress += 1;
    changed.recovery.as_mut().unwrap().manifest_digest = Digest::from_bytes([8; 32]);
    assert!(
        attached
            .validate_transition(&changed, Transition::Renew)
            .is_err()
    );
}
