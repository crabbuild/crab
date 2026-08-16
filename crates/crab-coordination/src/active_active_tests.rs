use crate::active_active::*;
use crate::error::CoordinationError;
use crate::write_coordinator::{
    CoordinatedRefUpdate, CoordinatorMaterializationGap, CoordinatorRepairSnapshot,
    ManagedCoordinatorProvider,
};

fn active_active_replication() -> ActiveActiveReplicationConfig {
    ActiveActiveReplicationConfig {
        mode: ActiveActiveMode::ActiveActive,
        coordinator: Some(ActiveActiveCoordinatorConfig {
            kind: ActiveActiveCoordinatorKind::Managed,
            url: "dynamodb://crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            consistency: ActiveActiveCoordinatorConsistency::Linearizable,
        }),
        writers: vec![
            ActiveActiveWriterConfig {
                name: "east".into(),
                url: "crab://primary/repo".into(),
                region: "us-east-1".into(),
                enabled: true,
            },
            ActiveActiveWriterConfig {
                name: "west".into(),
                url: "crab://west/repo".into(),
                region: "us-west-2".into(),
                enabled: true,
            },
            ActiveActiveWriterConfig {
                name: "disabled".into(),
                url: "crab://disabled/repo".into(),
                region: "us-central-1".into(),
                enabled: false,
            },
        ],
    }
}

fn ref_update(name: &str, expected: Option<&str>, new: Option<&str>) -> CoordinatedRefUpdate {
    CoordinatedRefUpdate {
        name: name.into(),
        expected: expected.map(str::to_owned),
        new: new.map(str::to_owned),
        force: false,
    }
}

#[test]
fn active_active_requires_coordinator_and_writer() {
    let mut replication = ActiveActiveReplicationConfig {
        mode: ActiveActiveMode::ActiveActive,
        ..ActiveActiveReplicationConfig::default()
    };

    assert!(validate_active_active_config(&replication).is_err());

    replication.coordinator = Some(ActiveActiveCoordinatorConfig {
        kind: ActiveActiveCoordinatorKind::Managed,
        url: "dynamodb://crab-coordinator".into(),
        region: "us-east-1".into(),
        failover_regions: vec!["us-west-2".into()],
        consistency: ActiveActiveCoordinatorConsistency::Linearizable,
    });
    replication.writers.push(ActiveActiveWriterConfig {
        name: "east".into(),
        url: "crab://primary/repo".into(),
        region: "us-east-1".into(),
        enabled: true,
    });

    assert!(validate_active_active_config(&replication).is_ok());
}

#[test]
fn active_active_coordinator_resource_parses_managed_urls() {
    for (url, provider, name) in [
        (
            "dynamodb://crab-coordinator",
            ManagedCoordinatorProvider::DynamoDb,
            "crab-coordinator",
        ),
        (
            "spanner://global-state",
            ManagedCoordinatorProvider::Spanner,
            "global-state",
        ),
        (
            "cosmosdb://crab-coordinator/",
            ManagedCoordinatorProvider::CosmosDb,
            "crab-coordinator",
        ),
    ] {
        let resource = active_active_coordinator_resource(url).unwrap();
        assert_eq!(resource.provider, provider);
        assert_eq!(resource.name, name);
    }
}

#[test]
fn active_active_coordinator_resource_rejects_invalid_urls() {
    for url in [
        "crab-coordinator",
        "s3://crab-coordinator",
        "dynamodb://",
        "spanner:///",
    ] {
        assert!(
            active_active_coordinator_resource(url).is_err(),
            "expected {url} to be rejected"
        );
    }
}

#[test]
fn active_active_push_plan_selects_first_enabled_writer() {
    let replication = active_active_replication();
    let refs = vec![ref_update("refs/heads/main", Some("old"), Some("new"))];

    let plan = plan_active_active_push(&replication, None, 42, refs, Vec::new()).unwrap();

    assert_eq!(plan.writer.name, "east");
    assert_eq!(plan.coordinator_url, "dynamodb://crab-coordinator");
    assert_eq!(plan.request.writer, "east");
    assert_eq!(plan.request.region, "us-east-1");
    assert_eq!(plan.request.manifest_generation, 42);
    assert_eq!(
        plan.request.target_regions,
        vec!["us-east-1".to_owned(), "us-west-2".to_owned()]
    );
    assert!(plan.request.operation_id.starts_with("crab-op-"));
}

#[test]
fn active_active_push_plan_uses_preferred_enabled_writer() {
    let replication = active_active_replication();

    let plan = plan_active_active_push(
        &replication,
        Some("west"),
        7,
        vec![ref_update("refs/heads/feature", None, Some("new"))],
        Vec::new(),
    )
    .unwrap();

    assert_eq!(plan.writer.name, "west");
    assert_eq!(plan.request.writer, "west");
    assert_eq!(plan.request.region, "us-west-2");
}

#[test]
fn active_active_push_plan_rejects_disabled_preferred_writer() {
    let replication = active_active_replication();

    let err = plan_active_active_push(
        &replication,
        Some("disabled"),
        7,
        vec![ref_update("refs/heads/main", Some("old"), Some("new"))],
        Vec::new(),
    )
    .unwrap_err();

    assert!(matches!(err, CoordinationError::Configuration { .. }));
}

#[test]
fn active_active_push_plan_rejects_empty_ref_set() {
    let replication = active_active_replication();

    let err = plan_active_active_push(&replication, None, 7, Vec::new(), Vec::new()).unwrap_err();

    assert!(matches!(err, CoordinationError::Configuration { .. }));
}

#[test]
fn active_active_push_operation_id_is_stable_for_ref_order() {
    let replication = active_active_replication();
    let refs = vec![
        ref_update("refs/heads/main", Some("a"), Some("b")),
        CoordinatedRefUpdate {
            force: true,
            ..ref_update("refs/tags/v1", None, Some("c"))
        },
    ];
    let mut reversed = refs.clone();
    reversed.reverse();

    let first = plan_active_active_push(&replication, Some("east"), 9, refs, Vec::new()).unwrap();
    let second =
        plan_active_active_push(&replication, Some("east"), 9, reversed, Vec::new()).unwrap();

    assert_eq!(first.request.operation_id, second.request.operation_id);
}

#[test]
fn active_active_push_operation_id_changes_for_uploaded_objects() {
    let replication = active_active_replication();
    let refs = vec![ref_update("refs/heads/main", Some("a"), Some("b"))];

    let first = plan_active_active_push(
        &replication,
        Some("east"),
        9,
        refs.clone(),
        vec!["xorbs/aa/one".to_owned()],
    )
    .unwrap();
    let second = plan_active_active_push(
        &replication,
        Some("east"),
        9,
        refs,
        vec!["xorbs/bb/two".to_owned()],
    )
    .unwrap();

    assert_ne!(first.request.operation_id, second.request.operation_id);
    assert_eq!(
        first.request.uploaded_objects,
        vec!["xorbs/aa/one".to_owned()]
    );
}

#[test]
fn active_active_writer_name_matches_remote_url() {
    let replication = active_active_replication();

    let writer =
        active_active_writer_name_for_remote(&replication, Some("crab://west/repo")).unwrap();

    assert_eq!(writer, "west");
}

#[test]
fn active_active_writer_name_rejects_disabled_remote_url() {
    let replication = active_active_replication();

    let err = active_active_writer_name_for_remote(&replication, Some("crab://disabled/repo"))
        .unwrap_err();

    assert!(matches!(err, CoordinationError::Configuration { .. }));
    assert!(err.to_string().contains("writer disabled is disabled"));
}

#[test]
fn active_active_repair_plan_maps_gaps_to_enabled_writers() {
    let replication = active_active_replication();
    let snapshot = CoordinatorRepairSnapshot {
        coordinator_epoch: 7,
        materialization_gaps: vec![
            CoordinatorMaterializationGap {
                operation_id: "op-2".into(),
                manifest_generation: 12,
                region: "us-west-2".into(),
                writer: "east".into(),
                source_region: "us-east-1".into(),
                refs: Vec::new(),
                uploaded_objects: Vec::new(),
            },
            CoordinatorMaterializationGap {
                operation_id: "op-1".into(),
                manifest_generation: 11,
                region: "us-east-1".into(),
                writer: "west".into(),
                source_region: "us-west-2".into(),
                refs: Vec::new(),
                uploaded_objects: Vec::new(),
            },
        ],
    };

    let plan = plan_active_active_repair(&replication, &snapshot).unwrap();

    assert_eq!(plan.coordinator_epoch, 7);
    assert_eq!(plan.actions.len(), 2);
    assert_eq!(plan.actions[0].operation_id, "op-1");
    assert_eq!(plan.actions[0].writer.name, "east");
    assert_eq!(plan.actions[1].operation_id, "op-2");
    assert_eq!(plan.actions[1].writer.name, "west");
}
