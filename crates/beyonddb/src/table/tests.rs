use super::*;

#[test]
fn table_class_quota_expires_each_change_at_the_thirty_day_boundary() {
    const DAY: i64 = 24 * 60 * 60 * 1_000;
    let mut table = TableRecord {
        table_class: TableClass::Standard,
        table_class_updates_ms: vec![DAY, 2 * DAY],
        placement: TablePlacement::Account,
        id: "test-generation".into(),
        created_at_ms: 0,
        table_name: "Test".into(),
        key_schema: Vec::new(),
        attribute_definitions: Vec::new(),
        local_secondary_indexes: Vec::new(),
        global_secondary_indexes: Vec::new(),
        billing_mode: BillingMode::PayPerRequest,
        provisioned_throughput: None,
        deletion_protection_enabled: false,
        pay_per_request_since_ms: Some(0),
    };
    let change = TableSettings {
        table_class: Some(TableClass::StandardInfrequentAccess),
        deletion_protection_enabled: Some(true),
        ..Default::default()
    };
    let before = table.clone();
    assert!(!change.clone().apply(&mut table, 31 * DAY - 1));
    assert_eq!(
        table, before,
        "a refused class update must not apply other settings"
    );
    assert!(change.apply(&mut table, 31 * DAY));
    assert_eq!(table.table_class_updates_ms, vec![2 * DAY, 31 * DAY]);
    let revert = TableSettings {
        table_class: Some(TableClass::Standard),
        ..Default::default()
    };
    assert!(!revert.clone().apply(&mut table, 32 * DAY - 1));
    assert!(revert.apply(&mut table, 32 * DAY));
    assert_eq!(table.table_class_updates_ms, vec![31 * DAY, 32 * DAY]);
}
